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

#[derive(Default)]
struct PendingNotifications {
    queue: VecDeque<LspNotification>,
    draining: bool,
}

impl NonBlockingNotificationSink {
    fn new(best_effort_tx: mpsc::Sender<LspNotification>) -> Self {
        Self {
            best_effort_tx,
            pending: Arc::new(Mutex::new(PendingNotifications::default())),
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

        let should_drain = {
            let Ok(mut pending) = self.pending.lock() else {
                warn!("Notification queue lock poisoned; preserving delivery is impossible");
                return;
            };
            pending.queue.push_back(notification);
            if pending.draining {
                false
            } else {
                pending.draining = true;
                true
            }
        };

        if should_drain {
            let tx = self.best_effort_tx.clone();
            let pending = Arc::clone(&self.pending);
            tokio::spawn(async move {
                loop {
                    let next = pending
                        .lock()
                        .ok()
                        .and_then(|mut state| state.queue.pop_front());
                    let Some(notification) = next else {
                        if let Ok(mut state) = pending.lock() {
                            state.draining = false;
                        }
                        break;
                    };
                    if tx.send(notification).await.is_err() {
                        warn!("Notification channel closed; clearing pending notifications");
                        if let Ok(mut state) = pending.lock() {
                            state.queue.clear();
                            state.draining = false;
                        }
                        break;
                    }
                }
            });
        }
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
}
