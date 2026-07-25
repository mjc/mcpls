//! Per-MCP-session delivery of project events.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;

use rmcp::model::ResourceUpdatedNotificationParam;
use rmcp::{Peer, RoleServer};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

use crate::bridge::resources::make_uri;
use crate::bridge::{ResourceSubscriptions, uri_to_path};
use crate::project::{ProjectEvent, ProjectHandle, ProjectId};

/// Convert an LSP file URI from a diagnostics event into the MCP resource URI
/// used by `resources/subscribe`.
pub fn diagnostics_resource_uri(uri: &str) -> Option<String> {
    let uri = uri.parse().ok()?;
    let path = uri_to_path(&uri)?;
    make_uri(&path).ok()
}

fn event_resource_uri(event: &ProjectEvent) -> Option<String> {
    match event {
        ProjectEvent::DiagnosticsUpdated { uri, .. } => diagnostics_resource_uri(uri),
        ProjectEvent::StatusChanged { .. } | ProjectEvent::ServerExited { .. } => None,
    }
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
        let task = tokio::spawn(async move {
            loop {
                match events.recv().await {
                    Ok(event) => {
                        let Some(resource_uri) = event_resource_uri(&event) else {
                            continue;
                        };
                        if !subscriptions.contains(&resource_uri).await {
                            continue;
                        }
                        if notifier
                            .notify_resource_updated(resource_uri)
                            .await
                            .is_err()
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
mod tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use tokio::sync::{broadcast, mpsc};

    use super::{SessionEventSink, SessionNotifier};
    use super::{diagnostics_resource_uri, event_resource_uri};
    use crate::project::{ProjectEvent, ProjectId, ProjectStatus};

    struct TestNotifier(mpsc::UnboundedSender<String>);

    #[async_trait]
    impl SessionNotifier for TestNotifier {
        async fn notify_resource_updated(&self, uri: String) -> Result<(), ()> {
            self.0.send(uri).map_err(|_| ())
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
        assert_eq!(
            event_resource_uri(&ProjectEvent::StatusChanged {
                status: ProjectStatus::Ready,
                last_error: None,
            }),
            None
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
}
