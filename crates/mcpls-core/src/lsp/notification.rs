//! Non-blocking delivery from the LSP response pump to project actors.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;
use tracing::warn;

use super::types::LspNotification;

/// Notification endpoint exposed to the LSP response pump.
///
/// Its API is deliberately synchronous. The bounded sender stays private to
/// this module, so response-reading code cannot await notification capacity.
pub(super) struct NonBlockingNotificationSink {
    best_effort_tx: mpsc::Sender<LspNotification>,
    pending: Arc<Mutex<PendingNotifications>>,
}

struct PendingNotifications {
    queue: VecDeque<LspNotification>,
    draining: bool,
    capacity: usize,
    dropped: usize,
    overflow_kind: Option<&'static str>,
}

impl NonBlockingNotificationSink {
    fn new(best_effort_tx: mpsc::Sender<LspNotification>) -> Self {
        let capacity = best_effort_tx.max_capacity().max(1);
        Self {
            best_effort_tx,
            pending: Arc::new(Mutex::new(PendingNotifications {
                queue: VecDeque::with_capacity(capacity),
                draining: false,
                capacity,
                dropped: 0,
                overflow_kind: None,
            })),
        }
    }

    pub(super) fn forward(&self, notification: LspNotification) {
        let notification = match self.best_effort_tx.try_send(notification) {
            Ok(()) => return,
            Err(mpsc::error::TrySendError::Full(notification)) => notification,
            Err(mpsc::error::TrySendError::Closed(_)) => {
                warn!("Notification channel closed; dropping notification after shutdown");
                return;
            }
        };

        let (should_drain, overflow_notice, queue_capacity) = {
            let Ok(mut pending) = self.pending.lock() else {
                warn!("Notification queue lock poisoned; preserving delivery is impossible");
                return;
            };
            let overflow_notice = if let Some(index) = pending
                .queue
                .iter()
                .position(|queued| can_coalesce(queued, &notification))
            {
                pending.queue[index] = notification;
                None
            } else if pending.queue.len() < pending.capacity {
                pending.queue.push_back(notification);
                None
            } else {
                pending.dropped = pending.dropped.saturating_add(1);
                let kind = pending
                    .overflow_kind
                    .unwrap_or_else(|| notification_kind(&notification));
                pending.overflow_kind = Some(kind);
                (pending.dropped == 1).then_some((kind, pending.dropped))
            };
            let should_drain = if pending.draining {
                false
            } else {
                pending.draining = true;
                true
            };
            (should_drain, overflow_notice, pending.capacity)
        };

        if let Some((kind, dropped_count)) = overflow_notice {
            warn!(
                notification_kind = kind,
                queue_capacity,
                dropped_count,
                "Notification queue full; dropping non-coalescible notifications"
            );
        }

        if should_drain {
            let tx = self.best_effort_tx.clone();
            let pending = Arc::clone(&self.pending);
            tokio::spawn(async move {
                loop {
                    let (next, completed_drops, queue_capacity) =
                        pending.lock().map_or((None, 0, 0), |mut state| {
                            let next = state.queue.pop_front();
                            if next.is_none() {
                                let completed_drops = state.dropped;
                                state.dropped = 0;
                                state.overflow_kind = None;
                                state.draining = false;
                                (next, completed_drops, state.capacity)
                            } else {
                                (next, 0, state.capacity)
                            }
                        });
                    let Some(notification) = next else {
                        if completed_drops > 1 {
                            warn!(
                                queue_capacity,
                                dropped_count = completed_drops,
                                "Notification queue overflow episode complete"
                            );
                        }
                        break;
                    };
                    if tx.send(notification).await.is_err() {
                        warn!("Notification channel closed; clearing pending notifications");
                        if let Ok(mut state) = pending.lock() {
                            state.queue.clear();
                            state.dropped = 0;
                            state.overflow_kind = None;
                            state.draining = false;
                        }
                        break;
                    }
                }
            });
        }
    }
}

fn can_coalesce(queued: &LspNotification, incoming: &LspNotification) -> bool {
    match (queued, incoming) {
        (
            LspNotification::PublishDiagnostics(queued),
            LspNotification::PublishDiagnostics(incoming),
        ) => queued.uri == incoming.uri,
        (LspNotification::ServerStatus(_), LspNotification::ServerStatus(_)) => true,
        (
            LspNotification::Progress { token: queued, .. },
            LspNotification::Progress {
                token: incoming, ..
            },
        ) => queued == incoming,
        _ => false,
    }
}

const fn notification_kind(notification: &LspNotification) -> &'static str {
    match notification {
        LspNotification::PublishDiagnostics(_) => "publish_diagnostics",
        LspNotification::LogMessage(_) => "log_message",
        LspNotification::ShowMessage(_) => "show_message",
        LspNotification::ServerStatus(_) => "server_status",
        LspNotification::Progress { .. } => "progress",
        LspNotification::Other { .. } => "other",
    }
}

pub(super) fn non_blocking_notification_channel(
    capacity: usize,
) -> (NonBlockingNotificationSink, mpsc::Receiver<LspNotification>) {
    let (tx, rx) = mpsc::channel(capacity);
    (NonBlockingNotificationSink::new(tx), rx)
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use crate::lsp::types::ServerStatusParams;
    use lsp_types::Uri;

    #[tokio::test]
    async fn full_channel_delivers_every_notification_in_order() {
        let (sink, mut receiver) = non_blocking_notification_channel(1);
        let first = LspNotification::ServerStatus(ServerStatusParams {
            health: "ok".to_string(),
            quiescent: false,
            message: Some("first".to_string()),
        });
        let second = LspNotification::ServerStatus(ServerStatusParams {
            health: "ok".to_string(),
            quiescent: true,
            message: Some("second".to_string()),
        });
        sink.forward(first);
        sink.forward(second);

        let first = receiver.recv().await.expect("first notification");
        let second = tokio::time::timeout(std::time::Duration::from_secs(1), receiver.recv())
            .await
            .expect("queued notification must be delivered")
            .expect("second notification");
        assert!(
            matches!(first, LspNotification::ServerStatus(params) if params.message.as_deref() == Some("first"))
        );
        assert!(
            matches!(second, LspNotification::ServerStatus(params) if params.message.as_deref() == Some("second"))
        );
    }

    #[tokio::test]
    async fn pending_queue_is_bounded_and_coalesces_diagnostics() {
        let (sink, _receiver) = non_blocking_notification_channel(1);
        let uri: Uri = "file:///workspace/main.rs".parse().unwrap();

        for version in 1..=10_000 {
            sink.forward(LspNotification::PublishDiagnostics(
                lsp_types::PublishDiagnosticsParams {
                    uri: uri.clone(),
                    version: Some(version),
                    diagnostics: Vec::new(),
                },
            ));
        }

        let pending = sink.pending.lock().unwrap();
        assert_eq!(pending.queue.len(), 1);
        assert!(matches!(
            pending.queue.front(),
            Some(LspNotification::PublishDiagnostics(params)) if params.version == Some(10_000)
        ));
        assert!(pending.queue.len() <= pending.capacity);
    }

    #[tokio::test]
    async fn pending_queue_is_bounded_for_non_coalescible_bursts() {
        let (sink, _receiver) = non_blocking_notification_channel(1);

        for index in 0..10_000 {
            sink.forward(LspNotification::LogMessage(lsp_types::LogMessageParams {
                typ: lsp_types::MessageType::INFO,
                message: format!("message-{index}"),
            }));
        }

        let pending = sink.pending.lock().unwrap();
        assert!(pending.queue.len() <= pending.capacity);
        assert!(pending.dropped > 0);
        assert_eq!(pending.overflow_kind, Some("log_message"));
    }
}
