//! Canonical-path coordination for concurrent workspace edit commits.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use tokio::sync::Notify;
use tokio::time::{Instant, sleep_until};

/// One path scope that a workspace edit intends to mutate.
///
/// Callers should supply canonical paths. MCPLS cannot canonicalize create
/// targets that do not exist yet, so this type deliberately performs no I/O.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct EditResource {
    path: PathBuf,
    directory: bool,
}

impl EditResource {
    /// Coordinate one exact file or path.
    pub fn exact(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            directory: false,
        }
    }

    /// Coordinate a directory and every path below it.
    pub fn directory(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            directory: true,
        }
    }

    /// Return the coordinated canonical path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn conflicts_with(&self, other: &Self) -> bool {
        self.path == other.path
            || (self.directory && other.path.starts_with(&self.path))
            || (other.directory && self.path.starts_with(&other.path))
    }
}

/// Expected contention with an edit that already owns an overlapping path.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("edit resources overlap active edit {blocking_owner}: {paths:?}")]
pub struct EditContention {
    blocking_owner: String,
    paths: Vec<PathBuf>,
}

impl EditContention {
    /// Return the opaque owner label supplied by the blocking edit.
    #[must_use]
    pub fn blocking_owner(&self) -> &str {
        &self.blocking_owner
    }

    /// Return requested paths that overlap the blocking edit.
    #[must_use]
    pub fn paths(&self) -> &[PathBuf] {
        &self.paths
    }
}

/// Shared coordinator for atomic path-set acquisition.
#[derive(Clone, Debug, Default)]
pub struct EditCoordinator {
    inner: Arc<CoordinatorInner>,
}

impl EditCoordinator {
    /// Create an empty coordinator.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Atomically acquire every requested resource without waiting.
    ///
    /// On contention, no requested resource is retained.
    ///
    /// # Errors
    ///
    /// Returns [`EditContention`] when an active edit owns an overlapping path.
    pub fn try_acquire(
        &self,
        owner: impl Into<String>,
        resources: impl IntoIterator<Item = EditResource>,
    ) -> Result<EditLease, EditContention> {
        self.try_acquire_owned(owner.into(), normalized_resources(resources))
    }

    /// Wait up to `max_wait` for every requested resource to become available.
    ///
    /// Each attempt still acquires the complete path set atomically. Timeout is
    /// reported as ordinary contention so MCP handlers can return `not_ready`.
    ///
    /// # Errors
    ///
    /// Returns the latest [`EditContention`] if the path set remains occupied
    /// for `max_wait`.
    pub async fn acquire_for(
        &self,
        owner: impl Into<String>,
        resources: impl IntoIterator<Item = EditResource>,
        max_wait: Duration,
    ) -> Result<EditLease, EditContention> {
        let owner = owner.into();
        let resources = normalized_resources(resources);
        let deadline = Instant::now() + max_wait;

        loop {
            let notified = self.inner.released.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            let contention = match self.try_acquire_owned(owner.clone(), resources.clone()) {
                Ok(lease) => return Ok(lease),
                Err(contention) => contention,
            };
            tokio::select! {
                () = &mut notified => {}
                () = sleep_until(deadline) => return Err(contention),
            }
        }
    }

    fn try_acquire_owned(
        &self,
        owner: String,
        resources: Vec<EditResource>,
    ) -> Result<EditLease, EditContention> {
        let mut state = lock_state(&self.inner.state);
        for active in &state.active {
            let paths = overlapping_paths(&resources, &active.resources);
            if !paths.is_empty() {
                return Err(EditContention {
                    blocking_owner: active.owner.clone(),
                    paths,
                });
            }
        }

        let token = Arc::new(());
        state.active.push(ActiveEdit {
            token: Arc::clone(&token),
            owner: owner.clone(),
            resources,
        });
        drop(state);
        Ok(EditLease {
            inner: Arc::clone(&self.inner),
            token,
            owner,
        })
    }
}

/// RAII ownership of one atomically acquired path set.
#[derive(Debug)]
#[must_use = "dropping the lease releases its edit resources"]
pub struct EditLease {
    inner: Arc<CoordinatorInner>,
    token: Arc<()>,
    owner: String,
}

impl EditLease {
    /// Return the opaque owner label supplied during acquisition.
    #[must_use]
    pub fn owner(&self) -> &str {
        &self.owner
    }
}

impl Drop for EditLease {
    fn drop(&mut self) {
        let removed = {
            let mut state = lock_state(&self.inner.state);
            state
                .active
                .iter()
                .position(|active| Arc::ptr_eq(&active.token, &self.token))
                .map(|position| state.active.swap_remove(position))
                .is_some()
        };
        if removed {
            self.inner.released.notify_waiters();
        }
    }
}

#[derive(Debug, Default)]
struct CoordinatorInner {
    state: Mutex<CoordinatorState>,
    released: Notify,
}

#[derive(Debug, Default)]
struct CoordinatorState {
    active: Vec<ActiveEdit>,
}

#[derive(Debug)]
struct ActiveEdit {
    token: Arc<()>,
    owner: String,
    resources: Vec<EditResource>,
}

fn normalized_resources(resources: impl IntoIterator<Item = EditResource>) -> Vec<EditResource> {
    let mut resources = resources.into_iter().collect::<Vec<_>>();
    resources.sort();
    resources.dedup();
    resources
}

fn overlapping_paths(requested: &[EditResource], active: &[EditResource]) -> Vec<PathBuf> {
    requested
        .iter()
        .filter(|requested| active.iter().any(|active| requested.conflicts_with(active)))
        .map(|resource| resource.path.clone())
        .collect()
}

fn lock_state(state: &Mutex<CoordinatorState>) -> MutexGuard<'_, CoordinatorState> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disjoint_paths_are_admitted_together() {
        let coordinator = EditCoordinator::new();
        let first = coordinator
            .try_acquire("first", [EditResource::exact("src/first.rs")])
            .expect("first edit should be admitted");
        let second = coordinator
            .try_acquire("second", [EditResource::exact("src/second.rs")])
            .expect("disjoint edit should be admitted while first is active");

        assert_eq!(first.owner(), "first");
        assert_eq!(second.owner(), "second");
    }

    #[test]
    fn an_exact_path_reports_contention() {
        let coordinator = EditCoordinator::new();
        let _first = coordinator
            .try_acquire("first", [EditResource::exact("src/lib.rs")])
            .unwrap();

        let contention = coordinator
            .try_acquire("second", [EditResource::exact("src/lib.rs")])
            .unwrap_err();

        assert_eq!(contention.blocking_owner(), "first");
        assert_eq!(contention.paths(), [PathBuf::from("src/lib.rs")]);
    }

    #[test]
    fn multi_path_acquisition_is_all_or_nothing() {
        let coordinator = EditCoordinator::new();
        let _blocker = coordinator
            .try_acquire("blocker", [EditResource::exact("src/taken.rs")])
            .unwrap();

        coordinator
            .try_acquire(
                "contender",
                [
                    EditResource::exact("src/free.rs"),
                    EditResource::exact("src/taken.rs"),
                ],
            )
            .unwrap_err();

        let _proof = coordinator
            .try_acquire("proof", [EditResource::exact("src/free.rs")])
            .expect("failed acquisition must not retain its disjoint path");
    }

    #[test]
    fn dropping_a_lease_releases_every_path() {
        let coordinator = EditCoordinator::new();
        let lease = coordinator
            .try_acquire(
                "first",
                [
                    EditResource::exact("src/first.rs"),
                    EditResource::exact("src/second.rs"),
                ],
            )
            .unwrap();
        assert!(
            coordinator
                .try_acquire("blocked", [EditResource::exact("src/second.rs")])
                .is_err()
        );

        drop(lease);

        let _next = coordinator
            .try_acquire("next", [EditResource::exact("src/second.rs")])
            .expect("dropping the lease should release its whole path set");
    }

    #[test]
    fn directory_scopes_conflict_with_descendants_in_either_direction() {
        let coordinator = EditCoordinator::new();
        let directory = coordinator
            .try_acquire("directory", [EditResource::directory("src/generated")])
            .unwrap();
        assert!(
            coordinator
                .try_acquire("descendant", [EditResource::exact("src/generated/api.rs")])
                .is_err()
        );
        let _sibling = coordinator
            .try_acquire("sibling", [EditResource::exact("src/manual/api.rs")])
            .expect("a sibling tree must stay independent");
        drop(directory);

        let _descendant = coordinator
            .try_acquire("descendant", [EditResource::exact("src/generated/api.rs")])
            .unwrap();
        assert!(
            coordinator
                .try_acquire("directory", [EditResource::directory("src/generated")])
                .is_err()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn bounded_wait_acquires_after_release() {
        let coordinator = EditCoordinator::new();
        let blocker = coordinator
            .try_acquire("blocker", [EditResource::exact("src/lib.rs")])
            .unwrap();
        let waiting = {
            let coordinator = coordinator.clone();
            tokio::spawn(async move {
                coordinator
                    .acquire_for(
                        "waiting",
                        [EditResource::exact("src/lib.rs")],
                        Duration::from_secs(5),
                    )
                    .await
            })
        };
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished());

        drop(blocker);

        let lease = waiting.await.unwrap().unwrap();
        assert_eq!(lease.owner(), "waiting");
    }

    #[tokio::test(start_paused = true)]
    async fn bounded_wait_returns_contention_at_its_deadline() {
        let coordinator = EditCoordinator::new();
        let _blocker = coordinator
            .try_acquire("blocker", [EditResource::exact("src/lib.rs")])
            .unwrap();
        let waiting = {
            let coordinator = coordinator.clone();
            tokio::spawn(async move {
                coordinator
                    .acquire_for(
                        "waiting",
                        [EditResource::exact("src/lib.rs")],
                        Duration::from_secs(5),
                    )
                    .await
            })
        };
        tokio::task::yield_now().await;

        tokio::time::advance(Duration::from_secs(5)).await;

        let contention = waiting.await.unwrap().unwrap_err();
        assert_eq!(contention.blocking_owner(), "blocker");
        assert_eq!(contention.paths(), [PathBuf::from("src/lib.rs")]);
    }
}
