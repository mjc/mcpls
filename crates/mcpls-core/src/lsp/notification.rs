//! Non-blocking delivery from the LSP response pump to project actors.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::mpsc;
use tracing::warn;

use super::types::LspNotification;

/// Notification endpoint exposed to the LSP response pump.
///
/// Its API is deliberately synchronous. The bounded sender stays private to
/// this module, so response-reading code cannot await notification capacity.
pub(super) struct NonBlockingNotificationSink {
    best_effort_tx: mpsc::Sender<LspNotification>,
    readiness_pending: Arc<AtomicBool>,
}

impl NonBlockingNotificationSink {
    fn new(best_effort_tx: mpsc::Sender<LspNotification>) -> Self {
        Self {
            best_effort_tx,
            readiness_pending: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(super) fn forward(&self, notification: LspNotification) {
        if notification.completes_initial_load() {
            if !self.readiness_pending.swap(true, Ordering::AcqRel) {
                let tx = self.best_effort_tx.clone();
                let pending = Arc::clone(&self.readiness_pending);
                tokio::spawn(async move {
                    if tx.send(notification).await.is_err() {
                        warn!("Notification channel closed, dropping readiness notification");
                    }
                    pending.store(false, Ordering::Release);
                });
            }
        } else {
            match self.best_effort_tx.try_send(notification) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(_)) => {
                    warn!("Notification channel full, dropping notification");
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    warn!("Notification channel closed, dropping notification");
                }
            }
        }
    }
}

pub(super) fn non_blocking_notification_channel(
    capacity: usize,
) -> (NonBlockingNotificationSink, mpsc::Receiver<LspNotification>) {
    let (tx, rx) = mpsc::channel(capacity);
    (NonBlockingNotificationSink::new(tx), rx)
}
