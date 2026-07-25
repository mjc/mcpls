//! Per-MCP-session delivery of project events.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;

use rmcp::model::ResourceUpdatedNotificationParam;
use rmcp::{Peer, RoleServer};
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;

use crate::bridge::resources::{ResourceUriError, make_uri, parse_uri};
use crate::bridge::{ResourceSubscriptions, uri_to_path};
use crate::project::{ProjectEvent, ProjectHandle, ProjectId};

const PROJECT_STATUS_PREFIX: &str = "mcpls-project-status:///";
const PROJECT_EVENTS_PREFIX: &str = "mcpls-project-events:///";

/// Encode a project identity as a subscribable MCP status resource URI.
pub fn project_status_resource_uri(project_id: &ProjectId) -> String {
    format!("{PROJECT_STATUS_PREFIX}{project_id}")
}

/// Decode a project status resource URI into its stable project identity.
pub fn parse_project_status_resource_uri(uri: &str) -> Option<ProjectId> {
    uri.strip_prefix(PROJECT_STATUS_PREFIX)
        .filter(|value| !value.is_empty() && !value.contains('/'))
        .and_then(|value| ProjectId::new(value.to_string()).ok())
}

/// Encode a project identity as a bounded event-history resource URI.
pub fn project_events_resource_uri(project_id: &ProjectId) -> String {
    format!("{PROJECT_EVENTS_PREFIX}{project_id}")
}

/// Decode a project event resource URI and optional polling cursor.
pub fn parse_project_events_resource_uri(uri: &str) -> Option<(ProjectId, Option<u64>)> {
    let value = uri.strip_prefix(PROJECT_EVENTS_PREFIX)?;
    let (id, query) = value
        .split_once('?')
        .map_or((value, None), |(id, query)| (id, Some(query)));
    if id.is_empty() || id.contains('/') {
        return None;
    }
    let cursor = query
        .and_then(|query| query.strip_prefix("since="))
        .and_then(|value| value.parse().ok());
    if query.is_some() && cursor.is_none() {
        return None;
    }
    Some((ProjectId::new(id.to_string()).ok()?, cursor))
}

/// Resource scopes understood by a session subscription.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionResource {
    /// Cached diagnostics for one absolute file path.
    Diagnostics(PathBuf),
    /// Lifecycle state for one registered project.
    ProjectStatus(ProjectId),
    /// Bounded ordered event history for one project and optional cursor.
    ProjectEvents {
        /// Stable project identity.
        project_id: ProjectId,
        /// Return only events newer than this cursor.
        cursor: Option<u64>,
    },
}

/// Parse either a diagnostics or project-status resource URI.
pub fn parse_session_resource_uri(uri: &str) -> Result<SessionResource, ResourceUriError> {
    if let Some(project_id) = parse_project_status_resource_uri(uri) {
        return Ok(SessionResource::ProjectStatus(project_id));
    }
    if let Some((project_id, cursor)) = parse_project_events_resource_uri(uri) {
        return Ok(SessionResource::ProjectEvents { project_id, cursor });
    }
    parse_uri(uri).map(SessionResource::Diagnostics)
}

/// Convert an LSP file URI from a diagnostics event into the MCP resource URI
/// used by `resources/subscribe`.
pub fn diagnostics_resource_uri(uri: &str) -> Option<String> {
    let uri = uri.parse().ok()?;
    let path = uri_to_path(&uri)?;
    make_uri(&path).ok()
}

fn event_resource_uris(project_id: &ProjectId, event: &ProjectEvent) -> Vec<String> {
    if !event.belongs_to(project_id) {
        return Vec::new();
    }
    let mut uris = vec![project_events_resource_uri(project_id)];
    match event {
        ProjectEvent::DiagnosticsUpdated { uri, .. } => {
            if let Some(uri) = diagnostics_resource_uri(uri) {
                uris.push(uri);
            }
        }
        ProjectEvent::StatusChanged { .. }
        | ProjectEvent::ServerExited { .. }
        | ProjectEvent::ProjectRemoved { .. } => {
            uris.push(project_status_resource_uri(project_id));
        }
        ProjectEvent::FilesChanged { .. } | ProjectEvent::EditApplied { .. } => {}
    }
    uris
}

#[async_trait::async_trait]
pub trait SessionNotifier: Send + Sync {
    async fn notify_resource_updated(&self, uri: String) -> Result<(), ()>;
}

struct PeerNotifier(Peer<RoleServer>);

#[async_trait::async_trait]
impl SessionNotifier for PeerNotifier {
    async fn notify_resource_updated(&self, uri: String) -> Result<(), ()> {
        self.0
            .notify_resource_updated(ResourceUpdatedNotificationParam::new(uri))
            .await
            .map_err(|_| ())
    }
}

/// Owns event-forwarding tasks for one MCP session.
pub struct SessionEventSink {
    subscriptions: Arc<ResourceSubscriptions>,
    tasks: Mutex<HashMap<ProjectId, JoinHandle<()>>>,
    project_resources: Arc<Mutex<HashMap<ProjectId, HashSet<String>>>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ForwardOutcome {
    Continue,
    Disconnect,
}

enum QueuedEvent {
    Event(ProjectEvent),
    Lagged { skipped: u64 },
}

impl SessionEventSink {
    pub(crate) fn new(subscriptions: Arc<ResourceSubscriptions>) -> Self {
        Self {
            subscriptions,
            tasks: Mutex::new(HashMap::new()),
            project_resources: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub(crate) fn track_subscription(&self, project_id: ProjectId, uri: String) {
        self.project_resources
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(project_id)
            .or_default()
            .insert(uri);
    }

    pub(crate) fn untrack_subscription(&self, uri: &str) {
        let empty_projects = {
            let mut resources = self
                .project_resources
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for subscriptions in resources.values_mut() {
                subscriptions.remove(uri);
            }
            let empty_projects = resources
                .iter()
                .filter(|(_, subscriptions)| subscriptions.is_empty())
                .map(|(project_id, _)| project_id.clone())
                .collect::<Vec<_>>();
            resources.retain(|_, subscriptions| !subscriptions.is_empty());
            empty_projects
        };

        for project_id in empty_projects {
            self.stop_project_task(&project_id);
        }
    }

    fn stop_project_task(&self, project_id: &ProjectId) {
        let task = self
            .tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(project_id);
        if let Some(task) = task {
            task.abort();
        }
    }

    fn stop_all_tasks(&mut self) {
        let tasks = self
            .tasks
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for task in tasks.drain().map(|(_, task)| task) {
            task.abort();
        }
    }

    /// Attach all actor groups for one logical project to this session's peer,
    /// deduplicating repeated subscriptions for resources owned by the project.
    pub(crate) fn attach(
        &self,
        project_id: ProjectId,
        actors: &[ProjectHandle],
        peer: Peer<RoleServer>,
    ) {
        self.attach_receivers(
            project_id,
            actors.iter().map(ProjectHandle::subscribe_events).collect(),
            Arc::new(PeerNotifier(peer)),
        );
    }

    fn attach_receivers(
        &self,
        project_id: ProjectId,
        event_receivers: Vec<broadcast::Receiver<ProjectEvent>>,
        notifier: Arc<dyn SessionNotifier>,
    ) {
        let mut tasks = self
            .tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if tasks
            .get(&project_id)
            .is_some_and(|task| !task.is_finished())
        {
            return;
        }
        tasks.remove(&project_id);

        let subscriptions = Arc::clone(&self.subscriptions);
        let project_resources = Arc::clone(&self.project_resources);
        let event_project_id = project_id.clone();
        let (event_tx, mut event_rx) = mpsc::channel(32);
        for mut events in event_receivers {
            let event_tx = event_tx.clone();
            tokio::spawn(async move {
                loop {
                    match events.recv().await {
                        Ok(event) => {
                            if event_tx.send(QueuedEvent::Event(event)).await.is_err() {
                                break;
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(skipped)) => {
                            if event_tx
                                .send(QueuedEvent::Lagged { skipped })
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
            });
        }
        drop(event_tx);
        let task = tokio::spawn(async move {
            while let Some(queued_event) = event_rx.recv().await {
                let (outcome, removed_event) = forward_queued_event(
                    queued_event,
                    &subscriptions,
                    notifier.as_ref(),
                    &event_project_id,
                )
                .await;
                if let Some(event) = removed_event {
                    cleanup_removed_project_subscriptions(
                        &subscriptions,
                        &project_resources,
                        &event_project_id,
                        &event,
                    )
                    .await;
                    break;
                }
                if outcome == ForwardOutcome::Disconnect {
                    break;
                }
            }
        });
        tasks.insert(project_id, task);
    }
}

async fn forward_queued_event(
    queued_event: QueuedEvent,
    subscriptions: &ResourceSubscriptions,
    notifier: &dyn SessionNotifier,
    project_id: &ProjectId,
) -> (ForwardOutcome, Option<ProjectEvent>) {
    match queued_event {
        QueuedEvent::Event(event) => {
            let outcome = forward_event(subscriptions, notifier, project_id, &event).await;
            let removed = matches!(event, ProjectEvent::ProjectRemoved { .. })
                && event.belongs_to(project_id);
            (outcome, removed.then_some(event))
        }
        QueuedEvent::Lagged { skipped } => {
            tracing::warn!(
                skipped,
                "session actor event sink lagged; notifying polling resync"
            );
            let outcome = notify_subscribed_resource(
                subscriptions,
                notifier,
                project_events_resource_uri(project_id),
            )
            .await;
            (outcome, None)
        }
    }
}

async fn forward_event(
    subscriptions: &ResourceSubscriptions,
    notifier: &dyn SessionNotifier,
    project_id: &ProjectId,
    event: &ProjectEvent,
) -> ForwardOutcome {
    for resource_uri in event_resource_uris(project_id, event) {
        if notify_subscribed_resource(subscriptions, notifier, resource_uri).await
            == ForwardOutcome::Disconnect
        {
            return ForwardOutcome::Disconnect;
        }
    }
    ForwardOutcome::Continue
}

async fn notify_subscribed_resource(
    subscriptions: &ResourceSubscriptions,
    notifier: &dyn SessionNotifier,
    resource_uri: String,
) -> ForwardOutcome {
    if !subscriptions.contains(&resource_uri).await {
        return ForwardOutcome::Continue;
    }
    notifier
        .notify_resource_updated(resource_uri)
        .await
        .map_or(ForwardOutcome::Disconnect, |()| ForwardOutcome::Continue)
}

async fn cleanup_removed_project_subscriptions(
    subscriptions: &ResourceSubscriptions,
    project_resources: &Mutex<HashMap<ProjectId, HashSet<String>>>,
    project_id: &ProjectId,
    event: &ProjectEvent,
) {
    let uris = project_resources
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(project_id)
        .unwrap_or_default();
    let had_tracked_resources = !uris.is_empty();
    for uri in uris {
        subscriptions.unsubscribe(&uri).await;
    }
    if !had_tracked_resources {
        for uri in event_resource_uris(project_id, event) {
            subscriptions.unsubscribe(&uri).await;
        }
    }
}

impl Drop for SessionEventSink {
    fn drop(&mut self) {
        self.stop_all_tasks();
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use tokio::sync::{broadcast, mpsc};
    use tokio::task::JoinHandle;

    use super::{SessionEventSink, SessionNotifier};
    use super::{
        SessionResource, diagnostics_resource_uri, event_resource_uris,
        parse_project_events_resource_uri, parse_project_status_resource_uri,
        parse_session_resource_uri, project_events_resource_uri, project_status_resource_uri,
    };
    use crate::project::{ProjectEvent, ProjectId, ProjectStatus};

    struct TestNotifier(mpsc::UnboundedSender<String>);

    #[async_trait]
    impl SessionNotifier for TestNotifier {
        async fn notify_resource_updated(&self, uri: String) -> Result<(), ()> {
            self.0.send(uri).map_err(|_| ())
        }
    }

    struct FailingNotifier;

    #[async_trait]
    impl SessionNotifier for FailingNotifier {
        async fn notify_resource_updated(&self, _uri: String) -> Result<(), ()> {
            Err(())
        }
    }

    #[test]
    fn diagnostics_events_map_to_subscribable_resource_uris() {
        assert_eq!(
            diagnostics_resource_uri("file:///workspace/src/main.rs"),
            Some("lsp-diagnostics:///workspace/src/main.rs".to_string())
        );
    }

    #[test]
    fn lifecycle_events_do_not_map_to_file_resources() {
        let project_id = ProjectId::new("a").unwrap();
        assert_eq!(
            event_resource_uris(
                &project_id,
                &ProjectEvent::StatusChanged {
                    status: ProjectStatus::Ready,
                    last_error: None,
                },
            ),
            vec![
                "mcpls-project-events:///a".to_string(),
                "mcpls-project-status:///a".to_string(),
            ]
        );
        assert_eq!(
            parse_project_status_resource_uri(&project_status_resource_uri(&project_id)),
            Some(project_id.clone())
        );
        assert_eq!(
            parse_session_resource_uri(&project_status_resource_uri(&ProjectId::new("a").unwrap()))
                .unwrap(),
            SessionResource::ProjectStatus(ProjectId::new("a").unwrap())
        );
        assert_eq!(
            parse_session_resource_uri("lsp-diagnostics:///workspace/a.rs").unwrap(),
            SessionResource::Diagnostics(std::path::PathBuf::from("/workspace/a.rs"))
        );
        assert_eq!(
            event_resource_uris(
                &project_id,
                &ProjectEvent::ProjectRemoved {
                    project_id: project_id.clone(),
                    root: std::path::PathBuf::from("/workspace"),
                },
            ),
            vec![
                "mcpls-project-events:///a".to_string(),
                "mcpls-project-status:///a".to_string(),
            ]
        );
        assert!(
            event_resource_uris(
                &project_id,
                &ProjectEvent::ProjectRemoved {
                    project_id: ProjectId::new("other").unwrap(),
                    root: std::path::PathBuf::from("/workspace/other"),
                },
            )
            .is_empty()
        );
    }

    #[test]
    fn project_event_resource_uri_carries_optional_poll_cursor() {
        let project_id = ProjectId::new("a").unwrap();
        let uri = project_events_resource_uri(&project_id);
        assert_eq!(uri, "mcpls-project-events:///a");
        assert_eq!(
            parse_project_events_resource_uri("mcpls-project-events:///a?since=7"),
            Some((project_id, Some(7)))
        );
    }

    #[tokio::test]
    async fn session_sinks_filter_shared_events_by_their_own_subscriptions() {
        let file_a = "lsp-diagnostics:///workspace/a.rs".to_string();
        let file_b = "lsp-diagnostics:///workspace/b.rs".to_string();
        let subscriptions_a = Arc::new(crate::bridge::ResourceSubscriptions::new());
        let subscriptions_b = Arc::new(crate::bridge::ResourceSubscriptions::new());
        subscriptions_a.subscribe(file_a.clone()).await.unwrap();
        subscriptions_b.subscribe(file_b.clone()).await.unwrap();

        let sink_a = SessionEventSink::new(Arc::clone(&subscriptions_a));
        let sink_b = SessionEventSink::new(Arc::clone(&subscriptions_b));
        let (events_tx, events_rx) = broadcast::channel(8);
        let (updates_a_tx, mut updates_a_rx) = mpsc::unbounded_channel();
        let (updates_b_tx, mut updates_b_rx) = mpsc::unbounded_channel();

        sink_a.attach_receivers(
            ProjectId::new("a").unwrap(),
            vec![events_rx.resubscribe()],
            Arc::new(TestNotifier(updates_a_tx)),
        );
        sink_b.attach_receivers(
            ProjectId::new("b").unwrap(),
            vec![events_rx],
            Arc::new(TestNotifier(updates_b_tx)),
        );

        events_tx
            .send(ProjectEvent::DiagnosticsUpdated {
                uri: "file:///workspace/a.rs".to_string(),
                version: Some(1),
                diagnostic_count: 1,
            })
            .unwrap();

        assert_eq!(updates_a_rx.recv().await.unwrap(), file_a);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), updates_b_rx.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn session_sink_fans_in_events_from_multiple_actor_groups() {
        let project_id = ProjectId::new("logical").unwrap();
        let resource = project_events_resource_uri(&project_id);
        let subscriptions = Arc::new(crate::bridge::ResourceSubscriptions::new());
        subscriptions.subscribe(resource.clone()).await.unwrap();
        let sink = SessionEventSink::new(subscriptions);
        let (first_tx, first_rx) = broadcast::channel(8);
        let (second_tx, second_rx) = broadcast::channel(8);
        let (updates_tx, mut updates_rx) = mpsc::unbounded_channel();

        sink.attach_receivers(
            project_id.clone(),
            vec![first_rx, second_rx],
            Arc::new(TestNotifier(updates_tx)),
        );
        second_tx
            .send(ProjectEvent::StatusChanged {
                status: ProjectStatus::Failed,
                last_error: Some("secondary actor failed".to_string()),
            })
            .unwrap();

        assert_eq!(updates_rx.recv().await.unwrap(), resource);
        drop(first_tx);
        drop(second_tx);
    }

    #[tokio::test]
    async fn session_sink_notifies_project_events_after_broadcast_lag() {
        let project_id = ProjectId::new("a").unwrap();
        let event_uri = project_events_resource_uri(&project_id);
        let subscriptions = Arc::new(crate::bridge::ResourceSubscriptions::new());
        subscriptions.subscribe(event_uri.clone()).await.unwrap();
        let sink = SessionEventSink::new(subscriptions);
        let (events_tx, events_rx) = broadcast::channel(1);
        let (updates_tx, mut updates_rx) = mpsc::unbounded_channel();

        sink.attach_receivers(
            project_id,
            vec![events_rx],
            Arc::new(TestNotifier(updates_tx)),
        );
        for generation in 1..=3 {
            events_tx
                .send(ProjectEvent::ServerExited { generation })
                .unwrap();
        }

        let first = tokio::time::timeout(std::time::Duration::from_secs(1), updates_rx.recv())
            .await
            .unwrap()
            .unwrap();
        let second = tokio::time::timeout(std::time::Duration::from_secs(1), updates_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first, event_uri);
        assert_eq!(second, event_uri);
    }

    #[tokio::test]
    async fn session_sink_stops_after_notifier_disconnects() {
        let uri = "lsp-diagnostics:///workspace/a.rs".to_string();
        let subscriptions = Arc::new(crate::bridge::ResourceSubscriptions::new());
        subscriptions.subscribe(uri).await.unwrap();
        let sink = SessionEventSink::new(subscriptions);
        let (events_tx, events_rx) = broadcast::channel(8);
        let project_id = ProjectId::new("a").unwrap();

        sink.attach_receivers(
            project_id.clone(),
            vec![events_rx],
            Arc::new(FailingNotifier),
        );
        events_tx
            .send(ProjectEvent::DiagnosticsUpdated {
                uri: "file:///workspace/a.rs".to_string(),
                version: Some(1),
                diagnostic_count: 1,
            })
            .unwrap();

        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if sink
                    .tasks
                    .lock()
                    .unwrap()
                    .get(&project_id)
                    .is_some_and(JoinHandle::is_finished)
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("session sink did not stop after peer disconnect");
    }

    #[tokio::test]
    async fn unsubscribing_last_project_resource_stops_sink() {
        let project_id = ProjectId::new("a").unwrap();
        let resource = project_events_resource_uri(&project_id);
        let subscriptions = Arc::new(crate::bridge::ResourceSubscriptions::new());
        subscriptions.subscribe(resource.clone()).await.unwrap();
        let sink = SessionEventSink::new(subscriptions);
        sink.track_subscription(project_id.clone(), resource);
        let (_events_tx, events_rx) = broadcast::channel(8);
        let (updates_tx, _updates_rx) = mpsc::unbounded_channel();

        sink.attach_receivers(
            project_id.clone(),
            vec![events_rx],
            Arc::new(TestNotifier(updates_tx)),
        );
        sink.untrack_subscription(&project_events_resource_uri(&project_id));

        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if sink
                    .tasks
                    .lock()
                    .unwrap()
                    .get(&project_id)
                    .is_none_or(JoinHandle::is_finished)
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("session sink did not stop after its last subscription was removed");
    }

    #[tokio::test]
    async fn removal_notification_cleans_project_subscriptions_and_sink() {
        let project_id = ProjectId::new("a").unwrap();
        let event_uri = project_events_resource_uri(&project_id);
        let status_uri = project_status_resource_uri(&project_id);
        let diagnostics_uri = "lsp-diagnostics:///workspace/src/main.rs".to_string();
        let other_diagnostics_uri = "lsp-diagnostics:///workspace/other/src/lib.rs".to_string();
        let subscriptions = Arc::new(crate::bridge::ResourceSubscriptions::new());
        subscriptions.subscribe(event_uri.clone()).await.unwrap();
        subscriptions.subscribe(status_uri.clone()).await.unwrap();
        subscriptions
            .subscribe(diagnostics_uri.clone())
            .await
            .unwrap();
        subscriptions
            .subscribe(other_diagnostics_uri.clone())
            .await
            .unwrap();
        let sink = SessionEventSink::new(Arc::clone(&subscriptions));
        sink.track_subscription(project_id.clone(), event_uri.clone());
        sink.track_subscription(project_id.clone(), status_uri.clone());
        sink.track_subscription(project_id.clone(), diagnostics_uri.clone());
        sink.track_subscription(
            ProjectId::new("other").unwrap(),
            other_diagnostics_uri.clone(),
        );
        let (events_tx, events_rx) = broadcast::channel(8);
        let (updates_tx, mut updates_rx) = mpsc::unbounded_channel();

        sink.attach_receivers(
            project_id.clone(),
            vec![events_rx],
            Arc::new(TestNotifier(updates_tx)),
        );
        events_tx
            .send(ProjectEvent::ProjectRemoved {
                project_id: project_id.clone(),
                root: std::path::PathBuf::from("/workspace"),
            })
            .unwrap();

        assert_eq!(updates_rx.recv().await.unwrap(), event_uri);
        assert_eq!(updates_rx.recv().await.unwrap(), status_uri);
        assert!(
            !subscriptions
                .contains(&project_events_resource_uri(&project_id))
                .await
        );
        assert!(
            !subscriptions
                .contains(&project_status_resource_uri(&project_id))
                .await
        );
        assert!(!subscriptions.contains(&diagnostics_uri).await);
        assert!(subscriptions.contains(&other_diagnostics_uri).await);
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if sink
                    .tasks
                    .lock()
                    .unwrap()
                    .get(&project_id)
                    .is_some_and(JoinHandle::is_finished)
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("session sink did not stop after project removal");
    }
}
