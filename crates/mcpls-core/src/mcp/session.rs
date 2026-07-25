//! Per-MCP-session delivery of project events.

use std::collections::HashMap;
use std::sync::Arc;

use rmcp::model::ResourceUpdatedNotificationParam;
use rmcp::{Peer, RoleServer};
use tokio::sync::Mutex;
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
    pub(crate) async fn attach(
        &self,
        project_id: ProjectId,
        actor: ProjectHandle,
        peer: Peer<RoleServer>,
    ) {
        let mut tasks = self.tasks.lock().await;
        if tasks
            .get(&project_id)
            .is_some_and(|task| !task.is_finished())
        {
            return;
        }
        tasks.remove(&project_id);

        let subscriptions = Arc::clone(&self.subscriptions);
        let mut events = actor.subscribe_events();
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
                        if peer
                            .notify_resource_updated(ResourceUpdatedNotificationParam::new(
                                resource_uri,
                            ))
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
        if let Ok(mut tasks) = self.tasks.try_lock() {
            for task in tasks.drain().map(|(_, task)| task) {
                task.abort();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{diagnostics_resource_uri, event_resource_uri};
    use crate::project::{ProjectEvent, ProjectStatus};

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
}
