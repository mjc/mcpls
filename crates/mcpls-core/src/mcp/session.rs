//! Per-MCP-session delivery of project events.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;

use rmcp::model::ResourceUpdatedNotificationParam;
use rmcp::{Peer, RoleServer};
use tokio::sync::broadcast;
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ForwardOutcome {
    Continue,
    Disconnect,
}

impl SessionEventSink {
    pub(crate) fn new(subscriptions: Arc<ResourceSubscriptions>) -> Self {
        Self {
            subscriptions,
            tasks: Mutex::new(HashMap::new()),
        }
    }

    /// Attach one project actor to this session's peer, deduplicating repeated
    /// subscriptions for files owned by the same project.
    pub(crate) fn attach(
        &self,
        project_id: ProjectId,
        actor: &ProjectHandle,
        peer: Peer<RoleServer>,
    ) {
        self.attach_receiver(
            project_id,
            actor.subscribe_events(),
            Arc::new(PeerNotifier(peer)),
        );
    }

    fn attach_receiver(
        &self,
        project_id: ProjectId,
        mut events: broadcast::Receiver<ProjectEvent>,
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
        let event_project_id = project_id.clone();
        let task = tokio::spawn(async move {
            loop {
                match events.recv().await {
                    Ok(event) => {
                        if forward_event(
                            &subscriptions,
                            notifier.as_ref(),
                            &event_project_id,
                            &event,
                        )
                        .await
                            == ForwardOutcome::Disconnect
                        {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                        tracing::warn!(skipped, "session event sink lagged; polling can resync");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        tasks.insert(project_id, task);
    }
}

async fn forward_event(
    subscriptions: &ResourceSubscriptions,
    notifier: &dyn SessionNotifier,
    project_id: &ProjectId,
    event: &ProjectEvent,
) -> ForwardOutcome {
    for resource_uri in event_resource_uris(project_id, event) {
        if !subscriptions.contains(&resource_uri).await {
            continue;
        }
        if notifier
            .notify_resource_updated(resource_uri)
            .await
            .is_err()
        {
            return ForwardOutcome::Disconnect;
        }
    }
    ForwardOutcome::Continue
}

impl Drop for SessionEventSink {
    fn drop(&mut self) {
        let tasks = self
            .tasks
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for task in tasks.drain().map(|(_, task)| task) {
            task.abort();
        }
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

        sink_a.attach_receiver(
            ProjectId::new("a").unwrap(),
            events_rx.resubscribe(),
            Arc::new(TestNotifier(updates_a_tx)),
        );
        sink_b.attach_receiver(
            ProjectId::new("b").unwrap(),
            events_rx,
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
    async fn session_sink_stops_after_notifier_disconnects() {
        let uri = "lsp-diagnostics:///workspace/a.rs".to_string();
        let subscriptions = Arc::new(crate::bridge::ResourceSubscriptions::new());
        subscriptions.subscribe(uri).await.unwrap();
        let sink = SessionEventSink::new(subscriptions);
        let (events_tx, events_rx) = broadcast::channel(8);
        let project_id = ProjectId::new("a").unwrap();

        sink.attach_receiver(project_id.clone(), events_rx, Arc::new(FailingNotifier));
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
}
