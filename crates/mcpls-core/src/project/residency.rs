use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use tokio::sync::{Mutex, Notify, mpsc, oneshot};

use super::ProjectRequest;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) struct RustGroupId(pub(super) u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResidencyDecision {
    Admit,
    Reuse,
    Evict(RustGroupId),
    Wait,
}

#[derive(Debug, Clone, Copy)]
struct ResidentGroup {
    pins: usize,
    last_used: Instant,
}

#[derive(Debug)]
struct RustResidencyBudget {
    limit: usize,
    idle_timeout: Duration,
    residents: HashMap<RustGroupId, ResidentGroup>,
}

const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(60 * 60);

impl RustResidencyBudget {
    fn with_idle_timeout(limit: usize, idle_timeout: Duration) -> Self {
        Self {
            limit: limit.max(1),
            idle_timeout,
            residents: HashMap::new(),
        }
    }

    fn pin(&mut self, group: RustGroupId, excluded: &HashSet<RustGroupId>) -> ResidencyDecision {
        self.pin_with_minimum_idle(group, excluded, self.idle_timeout)
    }

    fn pin_for_activation(
        &mut self,
        group: RustGroupId,
        excluded: &HashSet<RustGroupId>,
    ) -> ResidencyDecision {
        self.pin_with_minimum_idle(group, excluded, Duration::ZERO)
    }

    fn pin_with_minimum_idle(
        &mut self,
        group: RustGroupId,
        excluded: &HashSet<RustGroupId>,
        minimum_idle: Duration,
    ) -> ResidencyDecision {
        let now = Instant::now();
        if let Some(resident) = self.residents.get_mut(&group) {
            resident.pins = resident.pins.saturating_add(1);
            resident.last_used = now;
            return ResidencyDecision::Reuse;
        }
        if self.residents.len() < self.limit {
            self.residents.insert(
                group,
                ResidentGroup {
                    pins: 1,
                    last_used: now,
                },
            );
            return ResidencyDecision::Admit;
        }
        self.residents
            .iter()
            .filter(|(candidate, resident)| {
                resident.pins == 0
                    && !excluded.contains(candidate)
                    && resident.last_used.elapsed() >= minimum_idle
            })
            .min_by_key(|(_, resident)| resident.last_used)
            .map_or(ResidencyDecision::Wait, |(candidate, _)| {
                ResidencyDecision::Evict(*candidate)
            })
    }

    fn pin_existing(&mut self, group: RustGroupId) -> bool {
        let now = Instant::now();
        let Some(resident) = self.residents.get_mut(&group) else {
            return false;
        };
        resident.pins = resident.pins.saturating_add(1);
        resident.last_used = now;
        true
    }

    fn replace(&mut self, victim: RustGroupId, group: RustGroupId) {
        let now = Instant::now();
        self.residents.remove(&victim);
        self.residents.insert(
            group,
            ResidentGroup {
                pins: 1,
                last_used: now,
            },
        );
    }

    fn unpin(&mut self, group: RustGroupId) {
        if let Some(resident) = self.residents.get_mut(&group) {
            resident.pins = resident.pins.saturating_sub(1);
            resident.last_used = Instant::now();
        }
    }

    fn remove(&mut self, group: RustGroupId) {
        self.residents.remove(&group);
    }

    #[cfg(test)]
    fn resident_count(&self) -> usize {
        self.residents.len()
    }

    fn next_idle_delay(&self, excluded: &HashSet<RustGroupId>) -> Option<Duration> {
        self.residents
            .iter()
            .filter(|(candidate, resident)| resident.pins == 0 && !excluded.contains(candidate))
            .map(|(_, resident)| {
                self.idle_timeout
                    .checked_sub(resident.last_used.elapsed())
                    .unwrap_or_default()
            })
            .min()
    }
}

#[derive(Clone)]
pub(super) struct RustResidencyController {
    inner: Arc<RustResidencyInner>,
}

struct RustResidencyInner {
    state: StdMutex<RustResidencyState>,
    transition: Mutex<()>,
    changed: Notify,
}

struct RustResidencyState {
    budget: RustResidencyBudget,
    actors: HashMap<RustGroupId, mpsc::WeakSender<ProjectRequest>>,
}

impl RustResidencyController {
    pub(super) fn new(limit: usize) -> Self {
        Self::with_idle_timeout(limit, DEFAULT_IDLE_TIMEOUT)
    }

    fn with_idle_timeout(limit: usize, idle_timeout: Duration) -> Self {
        Self {
            inner: Arc::new(RustResidencyInner {
                state: StdMutex::new(RustResidencyState {
                    budget: RustResidencyBudget::with_idle_timeout(limit, idle_timeout),
                    actors: HashMap::new(),
                }),
                transition: Mutex::new(()),
                changed: Notify::new(),
            }),
        }
    }

    pub(super) fn register(&self, group: RustGroupId, sender: mpsc::WeakSender<ProjectRequest>) {
        self.inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .actors
            .insert(group, sender);
    }

    pub(super) async fn acquire(&self, group: RustGroupId) -> RustResidencyGuard {
        self.acquire_with_policy(group, false).await
    }

    pub(super) async fn acquire_for_activation(
        &self,
        group: RustGroupId,
    ) -> RustResidencyGuard {
        self.acquire_with_policy(group, true).await
    }

    async fn acquire_with_policy(
        &self,
        group: RustGroupId,
        evict_recent: bool,
    ) -> RustResidencyGuard {
        let mut excluded = HashSet::new();
        loop {
            let transition = self.inner.transition.lock().await;
            let decision = if evict_recent {
                self.state().budget.pin_for_activation(group, &excluded)
            } else {
                self.state().budget.pin(group, &excluded)
            };
            match decision {
                ResidencyDecision::Admit | ResidencyDecision::Reuse => {
                    return self.guard(group);
                }
                ResidencyDecision::Evict(victim) => {
                    let sender = self
                        .state()
                        .actors
                        .get(&victim)
                        .and_then(mpsc::WeakSender::upgrade);
                    let Some(sender) = sender else {
                        self.remove(victim);
                        continue;
                    };
                    let (reply, response) = oneshot::channel();
                    if sender
                        .send(ProjectRequest::Suspend { reply })
                        .await
                        .is_err()
                    {
                        self.remove(victim);
                        continue;
                    }
                    match response.await {
                        Ok(Ok(())) => {
                            self.state().budget.replace(victim, group);
                            return self.guard(group);
                        }
                        Ok(Err(())) => {
                            excluded.insert(victim);
                        }
                        Err(_) => self.remove(victim),
                    }
                }
                ResidencyDecision::Wait => {
                    let changed = self.inner.changed.notified();
                    tokio::pin!(changed);
                    changed.as_mut().enable();
                    let delay = self
                        .state()
                        .budget
                        .next_idle_delay(&excluded)
                        .unwrap_or(Duration::from_secs(1));
                    drop(transition);
                    tokio::select! {
                        () = changed => {}
                        () = tokio::time::sleep(delay) => {}
                    }
                    excluded.clear();
                    continue;
                }
            }
            drop(transition);
        }
    }

    pub(super) fn try_acquire_existing(&self, group: RustGroupId) -> Option<RustResidencyGuard> {
        self.state()
            .budget
            .pin_existing(group)
            .then(|| self.guard(group))
    }

    fn guard(&self, group: RustGroupId) -> RustResidencyGuard {
        RustResidencyGuard {
            group,
            inner: Arc::clone(&self.inner),
        }
    }

    pub(super) fn remove(&self, group: RustGroupId) {
        let mut state = self.state();
        state.budget.remove(group);
        state.actors.remove(&group);
        drop(state);
        self.inner.changed.notify_waiters();
    }

    fn state(&self) -> std::sync::MutexGuard<'_, RustResidencyState> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

pub(super) struct RustResidencyGuard {
    group: RustGroupId,
    inner: Arc<RustResidencyInner>,
}

impl Drop for RustResidencyGuard {
    fn drop(&mut self) {
        self.inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .budget
            .unpin(self.group);
        self.inner.changed.notify_waiters();
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    #[test]
    fn pinned_group_forces_second_cold_group_to_wait() {
        let mut budget = RustResidencyBudget::with_idle_timeout(1, Duration::ZERO);

        assert_eq!(
            budget.pin(RustGroupId(1), &HashSet::new()),
            ResidencyDecision::Admit
        );
        assert_eq!(
            budget.pin(RustGroupId(2), &HashSet::new()),
            ResidencyDecision::Wait
        );
        assert_eq!(budget.resident_count(), 1);
    }

    #[test]
    fn least_recently_used_unpinned_group_is_evicted() {
        let mut budget = RustResidencyBudget::with_idle_timeout(2, Duration::ZERO);
        let excluded = HashSet::new();

        assert_eq!(
            budget.pin(RustGroupId(1), &excluded),
            ResidencyDecision::Admit
        );
        budget.unpin(RustGroupId(1));
        assert_eq!(
            budget.pin(RustGroupId(2), &excluded),
            ResidencyDecision::Admit
        );
        budget.unpin(RustGroupId(2));

        assert_eq!(
            budget.pin(RustGroupId(3), &excluded),
            ResidencyDecision::Evict(RustGroupId(1))
        );
    }

    #[tokio::test]
    async fn unpinned_group_waits_for_idle_timeout_before_eviction() {
        let mut budget = RustResidencyBudget::with_idle_timeout(1, Duration::from_millis(20));
        let excluded = HashSet::new();
        assert_eq!(
            budget.pin(RustGroupId(1), &excluded),
            ResidencyDecision::Admit
        );
        budget.unpin(RustGroupId(1));
        assert_eq!(
            budget.pin(RustGroupId(2), &excluded),
            ResidencyDecision::Wait
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
        assert_eq!(
            budget.pin(RustGroupId(2), &excluded),
            ResidencyDecision::Evict(RustGroupId(1))
        );
    }

    #[tokio::test]
    async fn controller_suspends_unpinned_victim_before_admission() {
        let controller = RustResidencyController::with_idle_timeout(1, Duration::ZERO);
        let (first_sender, mut first_receiver) = mpsc::channel(1);
        let (second_sender, _second_receiver) = mpsc::channel(1);
        controller.register(RustGroupId(1), first_sender.downgrade());
        controller.register(RustGroupId(2), second_sender.downgrade());
        drop(controller.acquire(RustGroupId(1)).await);

        let suspension = tokio::spawn(async move {
            let Some(ProjectRequest::Suspend { reply }) = first_receiver.recv().await else {
                panic!("expected suspension request");
            };
            reply.send(Ok(())).unwrap();
        });
        let guard =
            tokio::time::timeout(Duration::from_secs(1), controller.acquire(RustGroupId(2)))
                .await
                .expect("second group should be admitted after suspension");

        suspension.await.unwrap();
        drop(guard);
    }

    #[tokio::test]
    async fn explicit_activation_evicts_a_recent_unpinned_group() {
        let controller =
            RustResidencyController::with_idle_timeout(1, Duration::from_secs(60 * 60));
        let (first_sender, mut first_receiver) = mpsc::channel(1);
        let (second_sender, _second_receiver) = mpsc::channel(1);
        controller.register(RustGroupId(1), first_sender.downgrade());
        controller.register(RustGroupId(2), second_sender.downgrade());
        drop(controller.acquire(RustGroupId(1)).await);

        let suspension = tokio::spawn(async move {
            let Some(ProjectRequest::Suspend { reply }) = first_receiver.recv().await else {
                panic!("expected suspension request");
            };
            reply.send(Ok(())).unwrap();
        });
        let guard = tokio::time::timeout(
            Duration::from_secs(1),
            controller.acquire_for_activation(RustGroupId(2)),
        )
        .await
        .expect("explicit activation should not wait for the idle timeout");

        suspension.await.unwrap();
        drop(guard);
    }

    #[tokio::test]
    async fn controller_queues_while_every_resident_group_is_pinned() {
        let controller = RustResidencyController::with_idle_timeout(1, Duration::ZERO);
        let (first_sender, mut first_receiver) = mpsc::channel(1);
        let (second_sender, _second_receiver) = mpsc::channel(1);
        controller.register(RustGroupId(1), first_sender.downgrade());
        controller.register(RustGroupId(2), second_sender.downgrade());
        let first_guard = controller.acquire(RustGroupId(1)).await;
        let second = tokio::spawn({
            let controller = controller.clone();
            async move { controller.acquire(RustGroupId(2)).await }
        });

        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!second.is_finished());
        drop(first_guard);
        let Some(ProjectRequest::Suspend { reply }) = first_receiver.recv().await else {
            panic!("expected suspension request");
        };
        reply.send(Ok(())).unwrap();

        let second_guard = tokio::time::timeout(Duration::from_secs(1), second)
            .await
            .expect("queued group should wake")
            .unwrap();
        drop(second_guard);
    }

    #[tokio::test]
    async fn controller_keeps_waiting_when_victim_refuses_suspension() {
        let controller = RustResidencyController::with_idle_timeout(1, Duration::ZERO);
        let (first_sender, mut first_receiver) = mpsc::channel(1);
        let (second_sender, _second_receiver) = mpsc::channel(1);
        controller.register(RustGroupId(1), first_sender.downgrade());
        controller.register(RustGroupId(2), second_sender.downgrade());
        drop(controller.acquire(RustGroupId(1)).await);
        let second = tokio::spawn({
            let controller = controller.clone();
            async move { controller.acquire(RustGroupId(2)).await }
        });

        let Some(ProjectRequest::Suspend { reply }) = first_receiver.recv().await else {
            panic!("expected suspension request");
        };
        reply.send(Err(())).unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!second.is_finished());

        controller.remove(RustGroupId(1));
        let second_guard = tokio::time::timeout(Duration::from_secs(1), second)
            .await
            .expect("queued group should wake after the pinned group leaves")
            .unwrap();
        drop(second_guard);
    }
}
