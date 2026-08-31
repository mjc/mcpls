//! LSP client implementation with async request/response handling.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::{Duration, Instant, timeout_at};
use tracing::{debug, debug_span, error, trace, warn};

use crate::config::LspServerConfig;
use crate::error::{Error, Result};
use crate::lsp::notification::NonBlockingNotificationSink;
#[cfg(test)]
use crate::lsp::notification::non_blocking_notification_channel;
use crate::lsp::transport::{LspReader, LspTransport, LspWriter};
use crate::lsp::types::{
    InboundMessage, JsonRpcError, JsonRpcRequest, JsonRpcResponse, LspNotification, RequestId,
};
use crate::lsp::watcher::{WATCH_EVENT_CHANNEL_CAPACITY, WatchRegistry, WatchSignal};

/// JSON-RPC protocol version.
const JSONRPC_VERSION: &str = "2.0";

/// LSP error code returned when the server cancels a request and wants the client to retry.
const SERVER_CANCELLED_CODE: i32 = -32802;

/// Maximum number of retry attempts for server-cancelled requests.
const SERVER_CANCELLED_MAX_RETRIES: u32 = 3;

/// Initial backoff delay for server-cancelled retries (milliseconds).
const SERVER_CANCELLED_INITIAL_DELAY_MS: u64 = 500;

/// Byte-length threshold for truncating an LSP error message before logging it.
///
/// Kept short since this feeds a single `tracing::error!` log line, not the
/// MCP caller -- see `MAX_ERROR_MESSAGE_CALLER_BYTES` for that budget.
const MAX_ERROR_MESSAGE_LOG_BYTES: usize = 200;

/// Byte-length threshold for the LSP error message forwarded to the MCP
/// caller in [`Error::LspServerError`] (#313).
///
/// Deliberately much larger than `MAX_ERROR_MESSAGE_LOG_BYTES`: a
/// legitimate LSP error (e.g. a verbose rust-analyzer type-mismatch
/// diagnostic reported through an error response) can run into the low
/// kilobytes, and that detail is useful to the calling model -- a log line
/// should stay terse, but a truncated-to-200-bytes error handed to the
/// model would cut off real content on every longer-but-honest error. Still
/// far below #311's 256 KiB cache-entry cap: this string is echoed directly
/// into the MCP tool result / model context, not merely cached.
const MAX_ERROR_MESSAGE_CALLER_BYTES: usize = 4 * 1024;

/// Upper bound on the effective timeout for completion requests, regardless
/// of `request_timeout_seconds`.
///
/// Completions are latency-sensitive: a completion list that takes longer
/// than this is no longer useful to the caller. This is a deliberate MVP
/// ceiling, not an oversight — completions cannot be configured above this
/// value today. See [`LspClient::completion_timeout`].
const COMPLETION_TIMEOUT_CAP: Duration = Duration::from_secs(10);

const OUTBOUND_TRANSPORT_QUEUE_CAPACITY: usize = 100;

/// Type alias for pending request tracking map.
type PendingRequests = HashMap<RequestId, oneshot::Sender<Result<Value>>>;

struct LspRequestTiming {
    span: tracing::Span,
    started: Instant,
}

impl LspRequestTiming {
    fn new(method: &str) -> Self {
        Self {
            span: debug_span!(
                "lsp.request",
                lsp_method = method,
                lsp_ms = tracing::field::Empty
            ),
            started: Instant::now(),
        }
    }
}

impl Drop for LspRequestTiming {
    fn drop(&mut self) {
        self.span
            .record("lsp_ms", self.started.elapsed().as_millis() as u64);
    }
}

/// LSP client with async request/response handling.
///
/// This client manages communication with an LSP server, handling:
/// - Concurrent requests with unique ID tracking
/// - Background message loop for receiving responses
/// - Timeout support for all requests
/// - Graceful shutdown
#[derive(Debug)]
pub struct LspClient {
    /// Configuration for this LSP server.
    config: LspServerConfig,

    /// Current server state.
    state: Arc<Mutex<super::ServerState>>,

    /// Atomic counter for request IDs.
    request_counter: Arc<AtomicI64>,

    /// Command sender for outbound messages.
    command_queue: LspCommandQueue,

    /// Requests awaiting a response, shared with the background message loop.
    ///
    /// Exposed here (not just captured by the loop) so [`Self::request`] can
    /// remove its own entry on timeout instead of leaking it, and so a
    /// connection known to be dead can fail its stragglers immediately via
    /// [`Self::fail_pending_requests`] rather than leaving each to discover
    /// that only when its own timeout elapses.
    pending_requests: Arc<Mutex<PendingRequests>>,

    /// Background receiver task handle.
    receiver_task: Option<JoinHandle<Result<()>>>,
}

impl Clone for LspClient {
    /// Creates a clone that shares the underlying connection.
    ///
    /// The clone does not own the receiver task and cannot perform shutdown.
    /// All clones share the same command channel for sending requests.
    fn clone(&self) -> Self {
        Self {
            config: self.config.clone(),
            state: Arc::clone(&self.state),
            request_counter: Arc::clone(&self.request_counter),
            command_queue: self.command_queue.clone(),
            pending_requests: Arc::clone(&self.pending_requests),
            receiver_task: None,
        }
    }
}

/// Commands for client control.
enum ClientCommand {
    /// Send a request and wait for response.
    SendRequest {
        request: JsonRpcRequest,
        response_tx: oneshot::Sender<Result<Value>>,
    },
    /// Send a notification (no response expected).
    SendNotification {
        method: String,
        params: Option<Value>,
        response_tx: oneshot::Sender<Result<()>>,
    },
    /// Rescan active dynamic watched-file registrations and flush matching notifications.
    SynchronizeWatchedFiles {
        changed_paths: Vec<PathBuf>,
        response_tx: oneshot::Sender<Result<(usize, usize)>>,
    },
    /// Cancel an in-flight request whose caller timed out.
    CancelRequest { id: RequestId },
    /// Shutdown the client.
    Shutdown,
}

#[derive(Clone, Debug)]
struct LspCommandQueue {
    sender: mpsc::Sender<ClientCommand>,
}

impl LspCommandQueue {
    const fn new(sender: mpsc::Sender<ClientCommand>) -> Self {
        Self { sender }
    }

    async fn send_until(
        &self,
        command: ClientCommand,
        deadline: Instant,
        timeout_duration: Duration,
    ) -> Result<()> {
        timeout_at(deadline, self.sender.send(command))
            .await
            .map_err(|_| Error::Timeout(timeout_duration.as_secs()))?
            .map_err(|_| Error::ServerTerminated)
    }

    fn try_send(&self, command: ClientCommand) {
        let _ = self.sender.try_send(command);
    }
}

struct OutboundBatch {
    messages: Vec<Value>,
    written_tx: Option<oneshot::Sender<Result<usize>>>,
}

impl OutboundBatch {
    fn one(message: Value) -> Self {
        Self {
            messages: vec![message],
            written_tx: None,
        }
    }
}

async fn lsp_writer_loop(
    mut writer: LspWriter,
    mut outbound_rx: mpsc::Receiver<OutboundBatch>,
) -> Result<()> {
    while let Some(batch) = outbound_rx.recv().await {
        let mut sent = 0usize;
        for message in &batch.messages {
            if let Err(error) = writer.send(message).await {
                if let Some(written_tx) = batch.written_tx {
                    let _ = written_tx.send(Err(Error::Transport(error.to_string())));
                }
                return Err(error);
            }
            sent = sent.saturating_add(1);
        }
        if let Some(written_tx) = batch.written_tx {
            let _ = written_tx.send(Ok(sent));
        }
    }
    Ok(())
}

fn enqueue_outbound(
    outbound_tx: &mpsc::Sender<OutboundBatch>,
    batch: OutboundBatch,
) -> std::result::Result<(), OutboundBatch> {
    outbound_tx
        .try_send(batch)
        .map_err(tokio::sync::mpsc::error::TrySendError::into_inner)
}

fn cancel_request_notification(id: &RequestId) -> Value {
    serde_json::json!({
        "jsonrpc": JSONRPC_VERSION,
        "method": "$/cancelRequest",
        "params": { "id": id },
    })
}

impl LspClient {
    /// Create a new LSP client with the given configuration.
    ///
    /// The client starts in an uninitialized state. Call `initialize()` to
    /// start the server and complete the initialization handshake.
    #[must_use]
    pub fn new(config: LspServerConfig) -> Self {
        // Placeholder channel - the receiver is intentionally dropped since
        // the client starts uninitialized. A real channel is created when
        // `from_transport` or `from_transport_with_notification_sink` is called.
        let (command_tx, _command_rx) = mpsc::channel(1); // Minimal capacity for placeholder

        Self {
            config,
            state: Arc::new(Mutex::new(super::ServerState::Uninitialized)),
            request_counter: Arc::new(AtomicI64::new(1)),
            command_queue: LspCommandQueue::new(command_tx),
            pending_requests: Arc::new(Mutex::new(HashMap::new())),
            receiver_task: None,
        }
    }

    /// Create client from transport (for testing or custom spawning).
    ///
    /// This method initializes the background message loop with the provided transport.
    #[cfg(test)]
    pub(crate) fn from_transport(config: LspServerConfig, transport: LspTransport) -> Self {
        let state = Arc::new(Mutex::new(super::ServerState::Initializing));
        let request_counter = Arc::new(AtomicI64::new(1));
        let pending_requests = Arc::new(Mutex::new(HashMap::new()));

        let (command_tx, command_rx) = mpsc::channel(100);

        let receiver_task = tokio::spawn(Self::message_loop(
            transport,
            command_rx,
            Arc::clone(&pending_requests),
            None,
            Vec::new(),
        ));

        Self {
            config,
            state,
            request_counter,
            command_queue: LspCommandQueue::new(command_tx),
            pending_requests,
            receiver_task: Some(receiver_task),
        }
    }

    /// Create client from transport with notification forwarding.
    ///
    /// Notifications received from the LSP server will be parsed and sent
    /// through the provided channel.
    pub(super) fn from_transport_with_notification_sink(
        config: LspServerConfig,
        transport: LspTransport,
        notification_sink: NonBlockingNotificationSink,
        workspace_roots: Vec<std::path::PathBuf>,
    ) -> Self {
        let state = Arc::new(Mutex::new(super::ServerState::Initializing));
        let request_counter = Arc::new(AtomicI64::new(1));
        let pending_requests = Arc::new(Mutex::new(HashMap::new()));

        let (command_tx, command_rx) = mpsc::channel(100);
        let receiver_task = tokio::spawn(Self::message_loop(
            transport,
            command_rx,
            Arc::clone(&pending_requests),
            Some(notification_sink),
            workspace_roots,
        ));

        Self {
            config,
            state,
            request_counter,
            command_queue: LspCommandQueue::new(command_tx),
            pending_requests,
            receiver_task: Some(receiver_task),
        }
    }

    /// Get the language ID for this client.
    #[must_use]
    pub fn language_id(&self) -> &str {
        &self.config.language_id
    }

    /// Get the current server state.
    pub async fn state(&self) -> super::ServerState {
        *self.state.lock().await
    }

    pub(crate) async fn set_ready(&self) {
        *self.state.lock().await = super::ServerState::Ready;
    }

    /// The timeout applied to a single LSP request attempt, derived from
    /// [`LspServerConfig::request_timeout_seconds`].
    ///
    /// This bounds one attempt, not a whole tool call: [`Self::request`]
    /// retries up to `SERVER_CANCELLED_MAX_RETRIES` (3) additional times on a
    /// `-32802` (`ServerCancelled`) response, so the worst-case latency for a
    /// single tool call is `4 * request_timeout() + 3.5s` (the sum of the
    /// retry backoff delays).
    ///
    /// The configured value is clamped to the range from 1 second to
    /// [`MAX_TIMEOUT_SECONDS`]. [`crate::serve`]/[`crate::serve_with`] now
    /// validate the top-level `ServerConfig` (via [`ServerConfig::validate`],
    /// which rejects `request_timeout_seconds` that is `0` or greater than
    /// [`MAX_TIMEOUT_SECONDS`]) regardless of whether it came from
    /// [`ServerConfig::load_from`] or was built programmatically by the
    /// caller. But `Self::new`, [`super::LspServer::spawn`], and
    /// [`super::LspServer::spawn_batch`] are all `pub` and take an
    /// [`LspServerConfig`] (or [`super::ServerInitConfig`] wrapping one)
    /// directly, bypassing that top-level validation entirely — it operates
    /// on the top-level `ServerConfig`, not the per-server one. This clamp is
    /// the last line of defense against a zero-duration timeout that would
    /// fail every request instantly, or an astronomically large one that
    /// tokio's `timeout`/`sleep` would silently treat as unbounded (they fall
    /// back to `Instant::far_future()` rather than panicking), for a caller
    /// reaching either of these levels directly.
    ///
    /// [`ServerConfig::load_from`]: crate::config::ServerConfig::load_from
    /// [`ServerConfig::validate`]: crate::config::ServerConfig::validate
    /// [`MAX_TIMEOUT_SECONDS`]: crate::config::MAX_TIMEOUT_SECONDS
    ///
    /// # Examples
    ///
    /// ```
    /// use std::time::Duration;
    /// use mcpls_core::config::LspServerConfig;
    /// use mcpls_core::lsp::LspClient;
    ///
    /// let mut config = LspServerConfig::rust_analyzer();
    /// config.request_timeout_seconds = 45;
    /// let client = LspClient::new(config);
    ///
    /// assert_eq!(client.request_timeout(), Duration::from_secs(45));
    /// ```
    #[must_use]
    pub fn request_timeout(&self) -> Duration {
        Duration::from_secs(
            self.config
                .request_timeout_seconds
                .clamp(1, crate::config::MAX_TIMEOUT_SECONDS),
        )
    }

    /// The timeout applied to completion (`textDocument/completion`) requests.
    ///
    /// Equal to [`Self::request_timeout`], capped at 10 seconds. Completions
    /// cannot be configured above this cap by any
    /// value of `request_timeout_seconds` — if that proves insufficient in
    /// practice, the fix is a dedicated `completion_timeout_seconds` field,
    /// not raising this cap.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::time::Duration;
    /// use mcpls_core::config::LspServerConfig;
    /// use mcpls_core::lsp::LspClient;
    ///
    /// let mut config = LspServerConfig::rust_analyzer();
    /// config.request_timeout_seconds = 300;
    /// let client = LspClient::new(config);
    ///
    /// // Capped at 10s even though request_timeout_seconds is 300.
    /// assert_eq!(client.completion_timeout(), Duration::from_secs(10));
    /// assert!(client.completion_timeout() <= client.request_timeout());
    /// ```
    #[must_use]
    pub fn completion_timeout(&self) -> Duration {
        self.request_timeout().min(COMPLETION_TIMEOUT_CAP)
    }

    /// Send request and wait for response with timeout.
    ///
    /// Automatically retries up to 3 times when the server returns error code
    /// -32802 (`ServerCancelled`) with `data.retriggerRequest == true`, using
    /// exponential backoff starting at 500 ms.
    ///
    /// # Type Parameters
    ///
    /// * `P` - The type of the request parameters (must be serializable)
    /// * `R` - The type of the response result (must be deserializable)
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - Server has shut down
    /// - Request times out
    /// - Response cannot be deserialized
    /// - LSP server returns an error
    pub async fn request<P, R>(
        &self,
        method: &str,
        params: P,
        timeout_duration: Duration,
    ) -> Result<R>
    where
        P: Serialize,
        R: DeserializeOwned,
    {
        let _timing = LspRequestTiming::new(method);
        let params_value = serde_json::to_value(params)?;
        let mut delay_ms = SERVER_CANCELLED_INITIAL_DELAY_MS;

        for attempt in 0..=SERVER_CANCELLED_MAX_RETRIES {
            if attempt > 0 {
                debug!(
                    "Retrying {} after ServerCancelled (attempt {}/{}), backoff={}ms",
                    method, attempt, SERVER_CANCELLED_MAX_RETRIES, delay_ms
                );
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                delay_ms *= 2;
            }

            let id = RequestId::Number(self.request_counter.fetch_add(1, Ordering::SeqCst));
            let (response_tx, response_rx) = oneshot::channel();
            let request = JsonRpcRequest {
                jsonrpc: JSONRPC_VERSION.to_string(),
                id: id.clone(),
                method: method.to_string(),
                params: Some(params_value.clone()),
            };

            debug!("Sending request: {} (id={:?})", method, id);

            let deadline = Instant::now() + timeout_duration;
            self.command_queue
                .send_until(
                    ClientCommand::SendRequest {
                        request,
                        response_tx,
                    },
                    deadline,
                    timeout_duration,
                )
                .await?;

            let outcome = match timeout_at(deadline, response_rx).await {
                Ok(received) => received.map_err(|_| Error::ServerTerminated)?,
                Err(_elapsed) => {
                    // Remove the pending sender before returning so a timed-out
                    // request cannot leak even when the outbound command queue
                    // is saturated. Cancellation stays detached so queue
                    // pressure cannot extend the caller's request deadline.
                    self.pending_requests.lock().await.remove(&id);
                    self.command_queue
                        .try_send(ClientCommand::CancelRequest { id });
                    return Err(Error::Timeout(timeout_duration.as_secs()));
                }
            };

            match outcome {
                Ok(result_value) => {
                    return serde_json::from_value(result_value).map_err(|e| {
                        Error::LspProtocolError(format!("Failed to deserialize response: {e}"))
                    });
                }
                Err(Error::LspServerError {
                    code,
                    ref message,
                    ref data,
                }) if code == SERVER_CANCELLED_CODE && Self::should_retrigger(data.as_ref()) => {
                    warn!(
                        "ServerCancelled (-32802) on '{}', will retry: {}",
                        method, message
                    );
                    if attempt == SERVER_CANCELLED_MAX_RETRIES {
                        return Err(Error::LspServerError {
                            code,
                            message: message.clone(),
                            data: data.clone(),
                        });
                    }
                    // continue loop for next attempt
                }
                Err(e) => return Err(e),
            }
        }

        Err(Error::ServerTerminated)
    }

    /// Returns true when the error data from a `ServerCancelled` (-32802) response
    /// indicates the server wants the client to retrigger the request.
    ///
    /// Per the LSP specification, `data.retriggerRequest == true` is the signal.
    /// When `data` is absent (older servers), we default to retrying anyway because
    /// code -32802 is exclusively used for this purpose.
    fn should_retrigger(data: Option<&Value>) -> bool {
        data.is_none_or(|v| {
            v.get("retriggerRequest")
                .and_then(Value::as_bool)
                .unwrap_or(true)
        })
    }

    /// Fail every request still parked in `pending_requests` with
    /// `Error::ServerTerminated`, instead of leaving each to discover a dead
    /// connection only when its own timeout elapses.
    ///
    /// Intended for a client that is about to be discarded -- e.g.
    /// superseded by a respawned replacement for the same server -- so
    /// callers still waiting on it unblock immediately.
    pub(crate) async fn fail_pending_requests(&self) {
        Self::fail_pending_map(&self.pending_requests).await;
    }

    async fn fail_pending_map(pending_requests: &Arc<Mutex<PendingRequests>>) {
        let mut pending = pending_requests.lock().await;
        for (_, sender) in pending.drain() {
            let _ = sender.send(Err(Error::ServerTerminated));
        }
    }

    /// Send notification (fire-and-forget, no response expected).
    ///
    /// # Errors
    ///
    /// Returns an error if the server has shut down.
    pub async fn notify<P>(&self, method: &str, params: P) -> Result<()>
    where
        P: Serialize,
    {
        let params_value = serde_json::to_value(params)?;
        let timeout_duration = self.request_timeout();
        let deadline = Instant::now() + timeout_duration;

        debug!("Sending notification: {}", method);

        let (response_tx, response_rx) = oneshot::channel();
        self.command_queue
            .send_until(
                ClientCommand::SendNotification {
                    method: method.to_string(),
                    params: Some(params_value),
                    response_tx,
                },
                deadline,
                timeout_duration,
            )
            .await?;
        timeout_at(deadline, response_rx)
            .await
            .map_err(|_| Error::Timeout(timeout_duration.as_secs()))?
            .map_err(|_| Error::ServerTerminated)??;

        Ok(())
    }

    /// Rescan active dynamic watched-file registrations, explicitly flush
    /// committed paths, and wait until every matching notification has been
    /// written to the provider transport.
    pub(crate) async fn synchronize_watched_files(
        &self,
        changed_paths: &[PathBuf],
        timeout_duration: Duration,
    ) -> Result<(usize, usize)> {
        let (response_tx, response_rx) = oneshot::channel();
        let deadline = Instant::now() + timeout_duration;
        self.command_queue
            .send_until(
                ClientCommand::SynchronizeWatchedFiles {
                    changed_paths: changed_paths.to_vec(),
                    response_tx,
                },
                deadline,
                timeout_duration,
            )
            .await?;
        timeout_at(deadline, response_rx)
            .await
            .map_err(|_| Error::Timeout(timeout_duration.as_secs()))?
            .map_err(|_| Error::ServerTerminated)?
    }

    /// Shutdown client gracefully.
    ///
    /// This sends a shutdown command to the background task and waits for it to complete.
    ///
    /// # Errors
    ///
    /// Returns an error if the background task failed.
    pub async fn shutdown(mut self) -> Result<()> {
        debug!("Shutting down LSP client");

        let timeout_duration = self.request_timeout();
        let _ = self
            .command_queue
            .send_until(
                ClientCommand::Shutdown,
                Instant::now() + timeout_duration,
                timeout_duration,
            )
            .await;

        if let Some(task) = self.receiver_task.take() {
            task.await
                .map_err(|e| Error::Transport(format!("Receiver task failed: {e}")))??;
        }

        *self.state.lock().await = super::ServerState::Shutdown;

        Ok(())
    }

    /// Background task: handle message I/O.
    ///
    /// This task runs in the background, handling:
    /// - Outbound requests and notifications
    /// - Inbound responses and server notifications
    /// - Matching responses to pending requests
    async fn message_loop(
        transport: LspTransport,
        mut command_rx: mpsc::Receiver<ClientCommand>,
        pending_requests: Arc<Mutex<PendingRequests>>,
        notification_sink: Option<NonBlockingNotificationSink>,
        workspace_roots: Vec<std::path::PathBuf>,
    ) -> Result<()> {
        debug!("Message loop started");
        let (writer, mut reader) = transport.split();
        let (outbound_tx, outbound_rx) = mpsc::channel(OUTBOUND_TRANSPORT_QUEUE_CAPACITY);
        let mut writer_task = tokio::spawn(lsp_writer_loop(writer, outbound_rx));
        let (watch_signal_tx, mut watch_signal_rx) = mpsc::channel(WATCH_EVENT_CHANNEL_CAPACITY);
        let mut watch_registry = WatchRegistry::new(workspace_roots, watch_signal_tx)
            .map_err(|error| Error::Transport(error.message))?;
        let result = Self::message_loop_inner(
            &mut reader,
            &mut command_rx,
            &pending_requests,
            notification_sink.as_ref(),
            &mut watch_registry,
            &mut watch_signal_rx,
            &outbound_tx,
        )
        .await;
        drop(outbound_tx);
        let writer_result = if let Ok(joined) =
            timeout_at(Instant::now() + Duration::from_secs(5), &mut writer_task).await
        {
            joined.map_err(|error| Error::Transport(format!("LSP writer task failed: {error}")))?
        } else {
            writer_task.abort();
            Err(Error::Timeout(5))
        };
        let result = result.and(writer_result);
        if let Err(ref e) = result {
            error!("Message loop exiting with error: {}", e);
        } else {
            debug!("Message loop exiting normally");
        }
        Self::fail_pending_map(&pending_requests).await;
        result
    }

    /// Truncate an LSP server's error message for the `tracing::error!` log
    /// line, bounding it to at most [`MAX_ERROR_MESSAGE_LOG_BYTES`] bytes
    /// (the full formatted string is slightly longer).
    ///
    /// Log-line use only -- the message forwarded to the MCP caller in
    /// [`Error::LspServerError`] is truncated separately, to the larger
    /// [`MAX_ERROR_MESSAGE_CALLER_BYTES`] (#313).
    fn truncate_error_message_for_log(message: &str) -> String {
        crate::util::truncate_str(message, MAX_ERROR_MESSAGE_LOG_BYTES)
    }

    #[allow(clippy::too_many_lines)]
    async fn message_loop_inner(
        reader: &mut LspReader,
        command_rx: &mut mpsc::Receiver<ClientCommand>,
        pending_requests: &Arc<Mutex<PendingRequests>>,
        notification_sink: Option<&NonBlockingNotificationSink>,
        watch_registry: &mut WatchRegistry,
        watch_signal_rx: &mut mpsc::Receiver<WatchSignal>,
        outbound_tx: &mpsc::Sender<OutboundBatch>,
    ) -> Result<()> {
        loop {
            tokio::select! {
                Some(command) = command_rx.recv() => {
                    match command {
                        ClientCommand::SendRequest { request, response_tx } => {
                            let id = request.id.clone();
                            pending_requests.lock().await.insert(
                                id.clone(),
                                response_tx,
                            );

                            let value = serde_json::to_value(&request)?;
                            if enqueue_outbound(outbound_tx, OutboundBatch::one(value)).is_err() {
                                let response_tx = pending_requests.lock().await.remove(&id);
                                if let Some(response_tx) = response_tx {
                                    let _ = response_tx.send(Err(Error::Transport(
                                        "outbound LSP transport queue is full or closed".to_string(),
                                    )));
                                }
                            }
                        }
                        ClientCommand::SendNotification { method, params, response_tx } => {
                            let notification = serde_json::json!({
                                "jsonrpc": "2.0",
                                "method": method,
                                "params": params,
                            });
                            let result = enqueue_outbound(
                                outbound_tx,
                                OutboundBatch::one(notification),
                            )
                            .map_err(|_| {
                                Error::Transport(
                                    "outbound LSP transport queue is full or closed".to_string(),
                                )
                            });
                            let _ = response_tx.send(result);
                        }
                        ClientCommand::SynchronizeWatchedFiles {
                            changed_paths,
                            response_tx,
                        } => {
                            let registrations = watch_registry.registration_count();
                            let events = match if changed_paths.is_empty() {
                                watch_registry.synchronize()
                            } else {
                                watch_registry.synchronize_paths(&changed_paths)
                            } {
                                Ok(events) => events,
                                Err(error) => {
                                    let _ = response_tx.send(Err(Error::Transport(error.message)));
                                    continue;
                                }
                            };
                            let messages = events
                                .into_iter()
                                .filter(|event| watch_registry.accepts(event))
                                .map(|event| serde_json::json!({
                                    "jsonrpc": JSONRPC_VERSION,
                                    "method": "workspace/didChangeWatchedFiles",
                                    "params": event.params,
                                }))
                                .collect::<Vec<_>>();
                            let (written_tx, written_rx) = oneshot::channel();
                            if enqueue_outbound(
                                outbound_tx,
                                OutboundBatch {
                                    messages,
                                    written_tx: Some(written_tx),
                                },
                            )
                            .is_err()
                            {
                                let _ = response_tx.send(Err(Error::Transport(
                                    "outbound LSP transport queue is full or closed".to_string(),
                                )));
                                continue;
                            }
                            tokio::spawn(async move {
                                let result = written_rx
                                    .await
                                    .map_err(|_| Error::ServerTerminated)
                                    .and_then(|result| result)
                                    .map(|sent| (registrations, sent));
                                let _ = response_tx.send(result);
                            });
                        }
                        ClientCommand::CancelRequest { id } => {
                            pending_requests.lock().await.remove(&id);
                            if enqueue_outbound(
                                outbound_tx,
                                OutboundBatch::one(cancel_request_notification(&id)),
                            )
                            .is_err()
                            {
                                warn!("Dropping LSP cancellation because the transport queue is full");
                            }
                        }
                        ClientCommand::Shutdown => {
                            debug!("Client shutdown requested");
                            break;
                        }
                    }
                }

                Some(signal) = watch_signal_rx.recv() => {
                    let events = match watch_registry.handle_signal(signal) {
                        Ok(events) => events,
                        Err(error) => {
                            warn!("Watched-file runtime degraded: {}", error.message);
                            continue;
                        }
                    };
                    let messages = events
                        .into_iter()
                        .filter(|event| watch_registry.accepts(event))
                        .map(|event| serde_json::json!({
                                "jsonrpc": JSONRPC_VERSION,
                                "method": "workspace/didChangeWatchedFiles",
                                "params": event.params,
                            }))
                        .collect::<Vec<_>>();
                    if !messages.is_empty()
                        && enqueue_outbound(
                            outbound_tx,
                            OutboundBatch {
                                messages,
                                written_tx: None,
                            },
                        )
                        .is_err()
                    {
                        warn!("Dropping watched-file notification because the transport queue is full");
                    }
                }

                message = reader.receive() => {
                    let message = match message {
                        Ok(m) => m,
                        Err(e) => {
                            error!("Transport receive error: {}", e);
                            return Err(e);
                        }
                    };
                    match message {
                        InboundMessage::Response(response) => {
                            trace!("Received response: id={:?}", response.id);

                            let sender = pending_requests.lock().await.remove(&response.id);

                            if let Some(sender) = sender {
                                if let Some(error) = response.error {
                                    let log_message = Self::truncate_error_message_for_log(&error.message);
                                    error!("LSP error response: {} (code {})", log_message, error.code);
                                    // Truncated separately from the log line, to the larger
                                    // MAX_ERROR_MESSAGE_CALLER_BYTES -- the raw message is
                                    // unbounded and attacker-influenceable (#313), but a
                                    // log-line-sized cut would also clip legitimate long
                                    // errors before the model ever sees them (S2).
                                    let caller_message = crate::util::truncate_str(
                                        &error.message,
                                        MAX_ERROR_MESSAGE_CALLER_BYTES,
                                    );
                                    let _ = sender.send(Err(Error::LspServerError {
                                        code: error.code,
                                        message: caller_message,
                                        data: error.data,
                                    }));
                                } else if let Some(result) = response.result {
                                    let _ = sender.send(Ok(result));
                                } else {
                                    // LSP spec allows null result for some requests (e.g., hover with no info).
                                    // Treat as successful response with null value.
                                    trace!("Response with null result: {:?}", response.id);
                                    let _ = sender.send(Ok(Value::Null));
                                }
                            } else {
                                warn!("Received response for unknown request ID: {:?}", response.id);
                            }
                        }
                        InboundMessage::Request(request) => {
                            debug!(
                                "Received server request: {} (id={:?})",
                                request.method, request.id
                            );
                            let response = Self::server_request_response_with_watchers(
                                request,
                                watch_registry,
                            );
                            let value = serde_json::to_value(&response)?;
                            if enqueue_outbound(outbound_tx, OutboundBatch::one(value)).is_err() {
                                warn!("Dropping LSP server response because the transport queue is full");
                            }
                        }
                        InboundMessage::Notification(notification) => {
                            debug!("Received notification: {}", notification.method);

                            // Parse notification into typed variant
                            let typed = LspNotification::parse(&notification.method, notification.params);

                            // Forward to notification handler if sender is available
                            if let Some(sink) = notification_sink {
                                // Progress reports are noisy and currently have no
                                // consumer. Preserve only the initial-indexing
                                // completion signal.
                                let readiness = typed.completes_initial_load();
                                if matches!(typed, LspNotification::Progress { .. }) && !readiness {
                                    continue;
                                }

                                // Log diagnostics count since it's useful for debugging
                                if let LspNotification::PublishDiagnostics(ref params) = typed {
                                    debug!(
                                        "Forwarding diagnostics for {}: {} items",
                                        params.uri.as_str(),
                                        params.diagnostics.len()
                                    );
                                } else {
                                    trace!("Forwarding notification: {:?}", typed);
                                }

                                sink.forward(typed);
                            }
                        }
                    }
                }
            }
        }

        Ok(())
    }

    #[cfg(test)]
    fn server_request_response(request: JsonRpcRequest) -> JsonRpcResponse {
        match Self::server_request_result(&request.method, request.params.as_ref()) {
            Ok(result) => JsonRpcResponse {
                jsonrpc: JSONRPC_VERSION.to_string(),
                id: request.id,
                result: Some(result),
                error: None,
            },
            Err(error) => JsonRpcResponse {
                jsonrpc: JSONRPC_VERSION.to_string(),
                id: request.id,
                result: None,
                error: Some(error),
            },
        }
    }

    fn server_request_response_with_watchers(
        request: JsonRpcRequest,
        watch_registry: &mut WatchRegistry,
    ) -> JsonRpcResponse {
        let result = match request.method.as_str() {
            "client/registerCapability" => watch_registry.register(request.params.as_ref()),
            "client/unregisterCapability" => watch_registry.unregister(request.params.as_ref()),
            _ => Self::server_request_result(&request.method, request.params.as_ref()),
        };
        match result {
            Ok(result) => JsonRpcResponse {
                jsonrpc: JSONRPC_VERSION.to_string(),
                id: request.id,
                result: Some(result),
                error: None,
            },
            Err(error) => JsonRpcResponse {
                jsonrpc: JSONRPC_VERSION.to_string(),
                id: request.id,
                result: None,
                error: Some(error),
            },
        }
    }

    fn server_request_result(
        method: &str,
        params: Option<&Value>,
    ) -> std::result::Result<Value, JsonRpcError> {
        match method {
            "workspace/workspaceFolders"
            | "workspace/diagnostic/refresh"
            | "workspace/semanticTokens/refresh"
            | "workspace/inlayHint/refresh"
            | "workspace/codeLens/refresh"
            | "window/showMessageRequest" => Ok(Value::Null),
            "workspace/configuration" => Ok(Self::workspace_configuration_result(params)),
            "workspace/applyEdit" => Ok(serde_json::json!({ "applied": false })),
            _ => Err(JsonRpcError {
                code: -32601,
                message: format!("Unhandled server request: {method}"),
                data: None,
            }),
        }
    }

    fn workspace_configuration_result(params: Option<&Value>) -> Value {
        let item_count = params
            .and_then(|value| value.get("items"))
            .and_then(Value::as_array)
            .map_or(0, Vec::len);

        Value::Array(vec![Value::Null; item_count])
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn test_request_id_generation() {
        let counter = AtomicI64::new(1);

        let id1 = counter.fetch_add(1, Ordering::SeqCst);
        let id2 = counter.fetch_add(1, Ordering::SeqCst);
        let id3 = counter.fetch_add(1, Ordering::SeqCst);

        assert_eq!(id1, 1);
        assert_eq!(id2, 2);
        assert_eq!(id3, 3);
    }

    #[test]
    fn test_client_creation() {
        let config = LspServerConfig::rust_analyzer();

        let client = LspClient::new(config);
        assert_eq!(client.language_id(), "rust");
    }

    #[test]
    fn lsp_request_span_uses_the_shared_provider_boundary() {
        let _subscriber = tracing::subscriber::set_default(tracing_subscriber::registry());
        let timing = LspRequestTiming::new("test/request");

        assert_eq!(timing.span.metadata().unwrap().name(), "lsp.request");
    }

    #[test]
    fn test_client_clone() {
        let config = LspServerConfig::rust_analyzer();
        let client = LspClient::new(config);

        #[allow(clippy::redundant_clone)]
        let cloned = client.clone();
        assert_eq!(cloned.language_id(), "rust");

        assert!(
            cloned.receiver_task.is_none(),
            "Cloned client should not own receiver task"
        );
    }

    #[test]
    fn test_request_timeout_and_completion_timeout_at_default() {
        let config = LspServerConfig::rust_analyzer();
        let client = LspClient::new(config);

        assert_eq!(client.request_timeout(), Duration::from_secs(30));
        assert_eq!(client.completion_timeout(), Duration::from_secs(10));
    }

    #[test]
    fn test_completion_timeout_clamps_to_ten_seconds() {
        for secs in [1, 2, 3, 30, 300] {
            let mut config = LspServerConfig::rust_analyzer();
            config.request_timeout_seconds = secs;
            let client = LspClient::new(config);

            assert_eq!(
                client.completion_timeout(),
                Duration::from_secs(secs.min(10)),
                "request_timeout_seconds={secs}"
            );
            assert!(client.completion_timeout() <= client.request_timeout());
        }
    }

    #[test]
    fn test_request_timeout_clamps_zero_to_one_second() {
        let mut config = LspServerConfig::rust_analyzer();
        config.request_timeout_seconds = 0;
        let client = LspClient::new(config);

        assert_eq!(client.request_timeout(), Duration::from_secs(1));
        assert_eq!(client.completion_timeout(), Duration::from_secs(1));
    }

    #[test]
    fn test_request_timeout_clamps_above_max_to_max() {
        let mut config = LspServerConfig::rust_analyzer();
        config.request_timeout_seconds = u64::MAX;
        let client = LspClient::new(config);

        assert_eq!(
            client.request_timeout(),
            Duration::from_secs(crate::config::MAX_TIMEOUT_SECONDS)
        );
    }

    #[test]
    fn test_request_timeout_independent_per_server() {
        let mut config_a = LspServerConfig::rust_analyzer();
        config_a.request_timeout_seconds = 5;
        let mut config_b = LspServerConfig::pyright();
        config_b.request_timeout_seconds = 15;

        let client_a = LspClient::new(config_a);
        let client_b = LspClient::new(config_b);

        assert_eq!(client_a.request_timeout(), Duration::from_secs(5));
        assert_eq!(client_b.request_timeout(), Duration::from_secs(15));
    }

    #[test]
    fn test_register_capability_request_is_acknowledged() {
        let request = JsonRpcRequest {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id: RequestId::String("ts1".to_string()),
            method: "client/registerCapability".to_string(),
            params: Some(serde_json::json!({ "registrations": [] })),
        };
        let (signal_tx, _signal_rx) = mpsc::channel(1);
        let mut registry = WatchRegistry::new(Vec::new(), signal_tx).unwrap();

        let response = LspClient::server_request_response_with_watchers(request, &mut registry);

        assert_eq!(response.id, RequestId::String("ts1".to_string()));
        assert_eq!(response.result, Some(Value::Null));
        assert!(response.error.is_none());
    }

    #[test]
    fn test_workspace_configuration_request_returns_null_per_item() {
        let result = LspClient::workspace_configuration_result(Some(&serde_json::json!({
            "items": [{ "section": "typescript" }, { "section": "editor" }]
        })));

        assert_eq!(result, serde_json::json!([null, null]));
    }

    #[test]
    fn test_unknown_server_request_returns_method_not_found() {
        let request = JsonRpcRequest {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id: RequestId::String("unknown-1".to_string()),
            method: "custom/request".to_string(),
            params: None,
        };

        let response = LspClient::server_request_response(request);

        assert!(response.result.is_none());
        match response.error {
            Some(error) => {
                assert_eq!(error.code, -32601);
                assert_eq!(error.message, "Unhandled server request: custom/request");
            }
            None => panic!("unknown request should return error"),
        }
    }

    #[tokio::test]
    async fn test_null_response_handling() {
        use crate::lsp::types::{JsonRpcResponse, RequestId};

        let pending_requests: Arc<Mutex<PendingRequests>> = Arc::new(Mutex::new(HashMap::new()));

        let (response_tx, response_rx) = oneshot::channel::<Result<Value>>();

        pending_requests
            .lock()
            .await
            .insert(RequestId::Number(1), response_tx);

        let null_response = JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: RequestId::Number(1),
            result: None,
            error: None,
        };

        let sender = pending_requests.lock().await.remove(&null_response.id);
        if let Some(sender) = sender {
            let _ = sender.send(Ok(Value::Null));
        }

        let timeout_result =
            tokio::time::timeout(tokio::time::Duration::from_millis(100), response_rx).await;

        assert!(timeout_result.is_ok(), "Should not timeout");

        let channel_result = timeout_result.unwrap();
        assert!(
            channel_result.is_ok(),
            "Channel should not be closed: {:?}",
            channel_result.err()
        );

        let response = channel_result.unwrap();
        assert!(
            response.is_ok(),
            "Should receive Ok(Value::Null), not Err: {:?}",
            response.err()
        );

        let value = response.unwrap();
        assert_eq!(value, Value::Null, "Should receive Value::Null");
    }

    #[tokio::test]
    async fn transport_exit_fails_pending_request_without_waiting_for_timeout() {
        use std::process::Stdio;

        let mut child = tokio::process::Command::new("sh")
            .args(["-c", "sleep 0.05"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let client = LspClient::from_transport(
            LspServerConfig::rust_analyzer(),
            LspTransport::new(stdin, stdout),
        );

        let result = tokio::time::timeout(
            Duration::from_secs(1),
            client.request::<_, Value>("test/request", Value::Null, Duration::from_secs(30)),
        )
        .await
        .unwrap();

        assert!(matches!(result, Err(Error::ServerTerminated)));
        let _ = child.wait().await;
    }

    #[tokio::test]
    async fn test_error_response_handling() {
        use crate::lsp::types::{JsonRpcError, JsonRpcResponse, RequestId};

        let pending_requests: Arc<Mutex<PendingRequests>> = Arc::new(Mutex::new(HashMap::new()));
        let (response_tx, response_rx) = oneshot::channel::<Result<Value>>();

        pending_requests
            .lock()
            .await
            .insert(RequestId::Number(1), response_tx);

        let error_response = JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: RequestId::Number(1),
            result: None,
            error: Some(JsonRpcError {
                code: -32601,
                message: "Method not found".to_string(),
                data: None,
            }),
        };

        let sender = pending_requests.lock().await.remove(&error_response.id);
        if let Some(sender) = sender
            && let Some(error) = error_response.error
        {
            let _ = sender.send(Err(Error::LspServerError {
                code: error.code,
                message: error.message,
                data: error.data,
            }));
        }

        let result = response_rx.await.unwrap();
        assert!(result.is_err(), "Should receive error");

        if let Err(Error::LspServerError { code, message, .. }) = result {
            assert_eq!(code, -32601);
            assert_eq!(message, "Method not found");
        } else {
            panic!("Expected LspServerError");
        }
    }

    #[tokio::test]
    async fn test_unknown_request_id() {
        use crate::lsp::types::{JsonRpcResponse, RequestId};

        let pending_requests: Arc<Mutex<PendingRequests>> = Arc::new(Mutex::new(HashMap::new()));

        let response = JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: RequestId::Number(999),
            result: Some(Value::Null),
            error: None,
        };

        let sender = pending_requests.lock().await.remove(&response.id);
        assert!(sender.is_none(), "Should not find sender for unknown ID");
    }

    #[test]
    fn test_truncate_error_message_for_log_handles_multibyte_boundary() {
        // 199 ASCII bytes followed by a 3-byte UTF-8 char ('€') straddles the byte-200 cut.
        let message = format!("{}€{}", "x".repeat(199), "y".repeat(50));

        let truncated = LspClient::truncate_error_message_for_log(&message);

        // Cutting before the multi-byte char keeps the message valid UTF-8 (no panic) and
        // pins the payload to 199 bytes, not 200.
        assert_eq!(truncated, format!("{}... (truncated)", "x".repeat(199)));
    }

    #[test]
    fn test_truncate_error_message_for_log_no_truncation_at_or_below_limit() {
        let exact = "x".repeat(200);
        assert_eq!(LspClient::truncate_error_message_for_log(&exact), exact);
        assert_eq!(LspClient::truncate_error_message_for_log(""), "");
    }

    #[test]
    fn test_truncate_error_message_for_log_truncates_just_above_limit() {
        let message = "x".repeat(201);
        assert_eq!(
            LspClient::truncate_error_message_for_log(&message),
            format!("{}... (truncated)", "x".repeat(200))
        );
    }

    #[test]
    fn test_truncate_error_message_for_log_handles_wide_char_at_limit() {
        // A 4-byte emoji run straddling every possible alignment near the byte-200 boundary.
        let message = format!("{}{}", "x".repeat(197), "🦀".repeat(10));

        let truncated = LspClient::truncate_error_message_for_log(&message);

        assert_eq!(truncated, format!("{}... (truncated)", "x".repeat(197)));
    }

    #[tokio::test]
    async fn test_concurrent_request_ids() {
        let counter = Arc::new(AtomicI64::new(1));

        let counter1 = Arc::clone(&counter);
        let counter2 = Arc::clone(&counter);
        let counter3 = Arc::clone(&counter);

        let handles = vec![
            tokio::spawn(async move { counter1.fetch_add(1, Ordering::SeqCst) }),
            tokio::spawn(async move { counter2.fetch_add(1, Ordering::SeqCst) }),
            tokio::spawn(async move { counter3.fetch_add(1, Ordering::SeqCst) }),
        ];

        let mut ids = Vec::new();
        for handle in handles {
            ids.push(handle.await.unwrap());
        }

        ids.sort_unstable();
        assert_eq!(ids, vec![1, 2, 3], "IDs should be unique and sequential");
    }

    #[test]
    fn test_jsonrpc_version_constant() {
        assert_eq!(JSONRPC_VERSION, "2.0");
    }

    /// #239 regression: a request that times out must remove its own entry
    /// from `pending_requests` instead of leaking it. `sleep` is used as the
    /// "server": it never writes anything to stdout, so no response can ever
    /// arrive and the request is guaranteed to time out rather than race a
    /// real answer.
    ///
    /// Unix-only: spawns a real `sleep` subprocess, which is unavailable on
    /// the Windows CI runner.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_request_timeout_removes_pending_entry() {
        let mut child = tokio::process::Command::new("sleep")
            .arg("2")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();

        let transport = LspTransport::new(stdin, stdout);
        let client = LspClient::from_transport(LspServerConfig::rust_analyzer(), transport);

        let result: Result<Value> = client
            .request(
                "textDocument/hover",
                serde_json::json!({}),
                Duration::from_millis(50),
            )
            .await;

        assert!(matches!(result, Err(Error::Timeout(_))), "got {result:?}");
        assert!(
            client.pending_requests.lock().await.is_empty(),
            "timed-out request must not remain in pending_requests"
        );
    }

    #[tokio::test]
    async fn request_timeout_includes_outbound_queue_admission() {
        let pending_requests = Arc::new(Mutex::new(HashMap::new()));
        let (command_tx, _command_rx) = mpsc::channel(1);
        command_tx.send(ClientCommand::Shutdown).await.unwrap();
        let client = LspClient {
            config: LspServerConfig::rust_analyzer(),
            state: Arc::new(Mutex::new(super::super::ServerState::Ready)),
            request_counter: Arc::new(AtomicI64::new(1)),
            command_queue: LspCommandQueue::new(command_tx),
            pending_requests,
            receiver_task: None,
        };

        let result = tokio::time::timeout(
            Duration::from_millis(200),
            client.request::<_, Value>(
                "textDocument/hover",
                serde_json::json!({}),
                Duration::from_millis(20),
            ),
        )
        .await;
        let Ok(result) = result else {
            panic!("the request deadline must include queue admission");
        };

        assert!(matches!(result, Err(Error::Timeout(_))), "got {result:?}");
    }

    /// #249 continuation: a client about to be discarded (e.g. superseded by
    /// a respawned replacement) must fail every still-pending request
    /// immediately rather than leaving callers to wait out their timeout.
    #[tokio::test]
    async fn test_fail_pending_requests_resolves_all_as_server_terminated() {
        let pending_requests: Arc<Mutex<PendingRequests>> = Arc::new(Mutex::new(HashMap::new()));
        let (command_tx, _command_rx) = mpsc::channel(1);

        let client = LspClient {
            config: LspServerConfig::rust_analyzer(),
            state: Arc::new(Mutex::new(super::super::ServerState::Ready)),
            request_counter: Arc::new(AtomicI64::new(1)),
            command_queue: LspCommandQueue::new(command_tx),
            pending_requests: Arc::clone(&pending_requests),
            receiver_task: None,
        };

        let (tx1, rx1) = oneshot::channel::<Result<Value>>();
        let (tx2, rx2) = oneshot::channel::<Result<Value>>();
        pending_requests
            .lock()
            .await
            .insert(RequestId::Number(1), tx1);
        pending_requests
            .lock()
            .await
            .insert(RequestId::Number(2), tx2);

        client.fail_pending_requests().await;

        assert!(pending_requests.lock().await.is_empty());
        assert!(matches!(rx1.await.unwrap(), Err(Error::ServerTerminated)));
        assert!(matches!(rx2.await.unwrap(), Err(Error::ServerTerminated)));
    }

    #[test]
    fn test_should_retrigger_defaults_to_true_when_data_absent() {
        assert!(LspClient::should_retrigger(None));
    }

    #[test]
    fn test_should_retrigger_false_when_flag_false() {
        assert!(!LspClient::should_retrigger(Some(&serde_json::json!({
            "retriggerRequest": false
        }))));
    }

    #[test]
    fn test_should_retrigger_true_when_flag_true() {
        assert!(LspClient::should_retrigger(Some(&serde_json::json!({
            "retriggerRequest": true
        }))));
    }

    mod retry_behavior {
        use std::process::Stdio;

        use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
        use tokio::process::{Child, ChildStdin, ChildStdout, Command};

        use super::*;
        use crate::config::LspServerConfig;

        struct FakeServer {
            _write_half: Child,
            _read_half: Child,
            read_half_stdin: ChildStdin,
            write_stdout: ChildStdout,
        }

        fn fake_lsp_transport() -> (LspTransport, FakeServer) {
            let mut write_half = Command::new("cat")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .unwrap();
            let write_stdin = write_half.stdin.take().unwrap();
            let write_stdout = write_half.stdout.take().unwrap();

            let mut read_half = Command::new("cat")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .unwrap();
            let read_stdout = read_half.stdout.take().unwrap();
            let read_stdin = read_half.stdin.take().unwrap();

            (
                LspTransport::new(write_stdin, read_stdout),
                FakeServer {
                    _write_half: write_half,
                    _read_half: read_half,
                    read_half_stdin: read_stdin,
                    write_stdout,
                },
            )
        }

        fn fake_lsp_client() -> (LspClient, FakeServer) {
            let (transport, server) = fake_lsp_transport();
            (
                LspClient::from_transport(LspServerConfig::rust_analyzer(), transport),
                server,
            )
        }

        fn fake_lsp_client_with_notifications()
        -> (LspClient, FakeServer, mpsc::Receiver<LspNotification>) {
            let (transport, server) = fake_lsp_transport();
            let (notification_sink, notification_rx) = non_blocking_notification_channel(1);
            let client = LspClient::from_transport_with_notification_sink(
                LspServerConfig::rust_analyzer(),
                transport,
                notification_sink,
                Vec::new(),
            );

            (client, server, notification_rx)
        }

        /// Reads one `Content-Length`-framed JSON-RPC message off `reader`.
        async fn read_framed_message(reader: &mut BufReader<&mut ChildStdout>) -> Value {
            let mut content_length = None;
            let mut line = String::new();
            loop {
                line.clear();
                reader.read_line(&mut line).await.unwrap();
                if line == "\r\n" || line == "\n" {
                    break;
                }
                if let Some((key, value)) = line.trim_end().split_once(':')
                    && key.trim().eq_ignore_ascii_case("content-length")
                {
                    content_length = Some(value.trim().parse::<usize>().unwrap());
                }
            }
            let mut buf = vec![0u8; content_length.unwrap()];
            reader.read_exact(&mut buf).await.unwrap();
            serde_json::from_slice(&buf).unwrap()
        }

        #[tokio::test]
        #[allow(clippy::expect_used)]
        async fn timed_out_request_returns_promptly_and_cancels_server_work() {
            let (client, mut server) = fake_lsp_client();
            let request_client = client.clone();
            let request_task = tokio::spawn(async move {
                request_client
                    .request::<_, Value>(
                        "textDocument/hover",
                        serde_json::json!({}),
                        Duration::from_millis(20),
                    )
                    .await
            });

            let mut reader = BufReader::new(&mut server.write_stdout);
            let request = read_framed_message(&mut reader).await;
            let request_id = request["id"].clone();
            let result = tokio::time::timeout(Duration::from_millis(200), request_task)
                .await
                .expect("request must return at its own deadline")
                .unwrap();
            assert!(matches!(result, Err(Error::Timeout(0))));

            let cancellation =
                tokio::time::timeout(Duration::from_secs(1), read_framed_message(&mut reader))
                    .await
                    .expect("timed-out request must send $/cancelRequest");
            assert_eq!(cancellation["method"], "$/cancelRequest");
            assert_eq!(cancellation["params"]["id"], request_id);
            assert!(client.pending_requests.lock().await.is_empty());
        }

        /// Writes a framed JSON-RPC `ServerCancelled` (-32802) error response.
        async fn write_server_cancelled_response(
            stdin: &mut ChildStdin,
            id: &Value,
            retrigger: bool,
        ) {
            let response = serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {
                    "code": SERVER_CANCELLED_CODE,
                    "message": "server cancelled the request",
                    "data": { "retriggerRequest": retrigger },
                },
            });
            let content = serde_json::to_string(&response).unwrap();
            let header = format!("Content-Length: {}\r\n\r\n", content.len());
            stdin.write_all(header.as_bytes()).await.unwrap();
            stdin.write_all(content.as_bytes()).await.unwrap();
            stdin.flush().await.unwrap();
        }

        /// Writes a framed JSON-RPC error response with an arbitrary code/message.
        async fn write_error_response(
            stdin: &mut ChildStdin,
            id: &Value,
            code: i32,
            message: &str,
        ) {
            let response = serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": code, "message": message },
            });
            let content = serde_json::to_string(&response).unwrap();
            let header = format!("Content-Length: {}\r\n\r\n", content.len());
            stdin.write_all(header.as_bytes()).await.unwrap();
            stdin.write_all(content.as_bytes()).await.unwrap();
            stdin.flush().await.unwrap();
        }

        /// Writes a framed JSON-RPC success response.
        async fn write_success_response(stdin: &mut ChildStdin, id: &Value, result: Value) {
            let response = serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": result,
            });
            let content = serde_json::to_string(&response).unwrap();
            let header = format!("Content-Length: {}\r\n\r\n", content.len());
            stdin.write_all(header.as_bytes()).await.unwrap();
            stdin.write_all(content.as_bytes()).await.unwrap();
            stdin.flush().await.unwrap();
        }

        async fn write_notification(stdin: &mut ChildStdin, method: &str, params: Value) {
            let notification = serde_json::json!({
                "jsonrpc": "2.0",
                "method": method,
                "params": params,
            });
            let content = serde_json::to_string(&notification).unwrap();
            let header = format!("Content-Length: {}\r\n\r\n", content.len());
            stdin.write_all(header.as_bytes()).await.unwrap();
            stdin.write_all(content.as_bytes()).await.unwrap();
            stdin.flush().await.unwrap();
        }

        #[tokio::test]
        #[allow(clippy::expect_used)]
        async fn full_notification_channel_does_not_block_lsp_responses() {
            let (client, mut server, _notification_rx) = fake_lsp_client_with_notifications();
            let request_task = tokio::spawn(async move {
                client
                    .request::<_, Value>(
                        "workspace/symbol",
                        serde_json::json!({ "query": "SettingsWriteOperation" }),
                        Duration::from_secs(30),
                    )
                    .await
            });
            let mut reader = BufReader::new(&mut server.write_stdout);
            let request = read_framed_message(&mut reader).await;

            for quiescent in [false, true] {
                write_notification(
                    &mut server.read_half_stdin,
                    "experimental/serverStatus",
                    serde_json::json!({
                        "health": "ok",
                        "quiescent": quiescent,
                    }),
                )
                .await;
            }
            write_success_response(
                &mut server.read_half_stdin,
                &request["id"],
                serde_json::json!([]),
            )
            .await;

            let result = tokio::time::timeout(Duration::from_millis(200), request_task)
                .await
                .expect("status backpressure must not stall the response pump")
                .unwrap()
                .unwrap();
            assert_eq!(result, serde_json::json!([]));
        }

        #[tokio::test]
        async fn blocked_transport_write_does_not_block_lsp_responses() {
            let (client, mut server) = fake_lsp_client();
            let request_client = client.clone();
            let request_task = tokio::spawn(async move {
                request_client
                    .request::<_, Value>(
                        "textDocument/hover",
                        serde_json::json!({}),
                        Duration::from_secs(1),
                    )
                    .await
            });
            let mut reader = BufReader::new(&mut server.write_stdout);
            let request = read_framed_message(&mut reader).await;

            client
                .notify(
                    "workspace/didChangeConfiguration",
                    serde_json::json!({ "blocked": "x".repeat(2 * 1024 * 1024) }),
                )
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(20)).await;
            write_success_response(
                &mut server.read_half_stdin,
                &request["id"],
                serde_json::json!({ "contents": "ready" }),
            )
            .await;

            let result = tokio::time::timeout(Duration::from_millis(200), request_task).await;
            let Ok(result) = result else {
                panic!("a blocked LSP write must not stop the response reader");
            };
            let result = result.unwrap().unwrap();
            assert_eq!(result["contents"], "ready");
        }

        #[tokio::test]
        async fn readiness_survives_notification_backpressure() {
            let (_client, mut server, mut notification_rx) = fake_lsp_client_with_notifications();
            write_notification(
                &mut server.read_half_stdin,
                "experimental/serverStatus",
                serde_json::json!({ "health": "ok", "quiescent": false }),
            )
            .await;
            write_notification(
                &mut server.read_half_stdin,
                "$/progress",
                serde_json::json!({
                    "token": "rustAnalyzer/Indexing",
                    "value": { "kind": "end" },
                }),
            )
            .await;

            let status = tokio::time::timeout(Duration::from_millis(200), notification_rx.recv())
                .await
                .unwrap()
                .unwrap();
            assert!(matches!(status, LspNotification::ServerStatus(_)));
            let readiness =
                tokio::time::timeout(Duration::from_millis(200), notification_rx.recv())
                    .await
                    .unwrap()
                    .unwrap();
            assert!(readiness.completes_initial_load());
        }

        #[tokio::test]
        async fn partial_inbound_frame_survives_outbound_command() {
            let (client, mut server) = fake_lsp_client();
            let request_client = client.clone();
            let request_task = tokio::spawn(async move {
                request_client
                    .request::<_, Value>(
                        "textDocument/hover",
                        serde_json::json!({}),
                        Duration::from_secs(1),
                    )
                    .await
            });

            let mut reader = BufReader::new(&mut server.write_stdout);
            let request = read_framed_message(&mut reader).await;
            let response = serde_json::json!({
                "jsonrpc": "2.0",
                "id": request["id"],
                "result": { "contents": "x".repeat(4096) },
            });
            let content = serde_json::to_vec(&response).unwrap();
            let header = format!("Content-Length: {}\r\n\r\n", content.len());
            let split = content.len() / 2;
            server
                .read_half_stdin
                .write_all(header.as_bytes())
                .await
                .unwrap();
            server
                .read_half_stdin
                .write_all(&content[..split])
                .await
                .unwrap();
            server.read_half_stdin.flush().await.unwrap();

            tokio::time::sleep(Duration::from_millis(20)).await;
            client
                .notify("workspace/didChangeConfiguration", serde_json::json!({}))
                .await
                .unwrap();
            let Ok(notification) =
                tokio::time::timeout(Duration::from_secs(1), read_framed_message(&mut reader))
                    .await
            else {
                panic!("outbound command must interrupt the pending receive");
            };
            assert_eq!(notification["method"], "workspace/didChangeConfiguration");

            server
                .read_half_stdin
                .write_all(&content[split..])
                .await
                .unwrap();
            server.read_half_stdin.flush().await.unwrap();

            let result = request_task.await.unwrap().unwrap();
            assert_eq!(result, response["result"]);
        }

        // Not `start_paused`: the retry loop's real backoff sleeps
        // interleave with real subprocess pipe I/O below, and paused
        // virtual time does not reliably auto-advance across both.
        #[tokio::test]
        async fn test_retry_exhaustion_returns_original_server_cancelled_error() {
            let (client, mut server) = fake_lsp_client();

            let request_task = tokio::spawn(async move {
                client
                    .request::<_, Value>(
                        "textDocument/hover",
                        serde_json::json!({}),
                        Duration::from_secs(30),
                    )
                    .await
            });

            let mut reader = BufReader::new(&mut server.write_stdout);
            // Initial attempt plus SERVER_CANCELLED_MAX_RETRIES retries: every
            // attempt gets ServerCancelled, so retries must exhaust rather
            // than loop forever or swallow the error.
            for _ in 0..=SERVER_CANCELLED_MAX_RETRIES {
                let request = read_framed_message(&mut reader).await;
                let id = request["id"].clone();
                write_server_cancelled_response(&mut server.read_half_stdin, &id, true).await;
            }

            let result = request_task.await.unwrap();

            match result {
                Err(Error::LspServerError {
                    code,
                    message,
                    data,
                }) => {
                    // Assert the exact original error surfaces, not merely
                    // "some error with this code" -- a freshly constructed
                    // placeholder error would satisfy a code-only check.
                    assert_eq!(code, SERVER_CANCELLED_CODE);
                    assert_eq!(message, "server cancelled the request");
                    assert_eq!(data, Some(serde_json::json!({ "retriggerRequest": true })));
                }
                other => panic!("expected exhausted ServerCancelled error, got {other:?}"),
            }
        }

        #[tokio::test]
        async fn test_retrigger_false_returns_immediately_without_retry() {
            let (client, mut server) = fake_lsp_client();

            let request_task = tokio::spawn(async move {
                client
                    .request::<_, Value>(
                        "textDocument/hover",
                        serde_json::json!({}),
                        Duration::from_secs(30),
                    )
                    .await
            });

            let mut reader = BufReader::new(&mut server.write_stdout);
            let request = read_framed_message(&mut reader).await;
            let id = request["id"].clone();
            write_server_cancelled_response(&mut server.read_half_stdin, &id, false).await;

            // With `retriggerRequest: false`, `should_retrigger`'s gate on
            // the retry branch must short-circuit the loop: the error
            // returns well under the first 500ms backoff, and no second
            // request is ever sent. If the `&& Self::should_retrigger(..)`
            // guard were ever dropped from the retry match arm, this would
            // instead retry and both assertions below would fail.
            let result = tokio::time::timeout(Duration::from_millis(200), request_task)
                .await
                .unwrap()
                .unwrap();

            match result {
                Err(Error::LspServerError { code, .. }) => {
                    assert_eq!(code, SERVER_CANCELLED_CODE);
                }
                other => panic!("expected immediate ServerCancelled error, got {other:?}"),
            }

            let second_request =
                tokio::time::timeout(Duration::from_millis(200), read_framed_message(&mut reader))
                    .await;
            assert!(
                second_request.is_err(),
                "no retry should have been sent after retriggerRequest: false"
            );
        }

        #[tokio::test]
        async fn test_retry_succeeds_after_one_server_cancelled_response() {
            let (client, mut server) = fake_lsp_client();

            let request_task = tokio::spawn(async move {
                client
                    .request::<_, Value>(
                        "textDocument/hover",
                        serde_json::json!({}),
                        Duration::from_secs(30),
                    )
                    .await
            });

            let mut reader = BufReader::new(&mut server.write_stdout);

            // First attempt is cancelled and must retrigger.
            let first = read_framed_message(&mut reader).await;
            write_server_cancelled_response(
                &mut server.read_half_stdin,
                &first["id"].clone(),
                true,
            )
            .await;

            // Second attempt (after backoff) succeeds -- proves the loop
            // genuinely re-sends the request rather than just counting down.
            let second = read_framed_message(&mut reader).await;
            assert_ne!(
                first["id"], second["id"],
                "retry must use a fresh request id"
            );
            let expected_result = serde_json::json!({ "contents": "resolved on retry" });
            write_success_response(
                &mut server.read_half_stdin,
                &second["id"].clone(),
                expected_result.clone(),
            )
            .await;

            let result = request_task.await.unwrap();
            assert_eq!(result.unwrap(), expected_result);
        }

        /// #313: an oversized, server-controlled error message must be
        /// truncated before it reaches the MCP caller in
        /// `Error::LspServerError`, not just before it is logged. Routes
        /// through the real `message_loop_inner` (via `fake_lsp_client`)
        /// rather than constructing the error by hand, so it actually
        /// exercises the fix.
        #[tokio::test]
        async fn test_oversized_error_message_truncated_for_caller() {
            let (client, mut server) = fake_lsp_client();

            let request_task = tokio::spawn(async move {
                client
                    .request::<_, Value>(
                        "textDocument/hover",
                        serde_json::json!({}),
                        Duration::from_secs(30),
                    )
                    .await
            });

            let mut reader = BufReader::new(&mut server.write_stdout);
            let request = read_framed_message(&mut reader).await;
            let id = request["id"].clone();
            let oversized_message = "x".repeat(MAX_ERROR_MESSAGE_CALLER_BYTES + 500);
            write_error_response(&mut server.read_half_stdin, &id, -32603, &oversized_message)
                .await;

            let result = request_task.await.unwrap();

            match result {
                Err(Error::LspServerError { code, message, .. }) => {
                    assert_eq!(code, -32603);
                    assert!(
                        message.len() < oversized_message.len(),
                        "caller-facing message must be truncated, got {} bytes",
                        message.len()
                    );
                    assert!(message.ends_with("... (truncated)"));
                }
                other => panic!("expected truncated LspServerError, got {other:?}"),
            }
        }

        /// #313 S2: a legitimate error message longer than the log-line cap
        /// (`MAX_ERROR_MESSAGE_LOG_BYTES`, 200 bytes) but shorter than the
        /// caller-facing cap must reach the MCP caller intact -- the
        /// caller-facing budget must not silently collapse to the log
        /// budget.
        #[tokio::test]
        async fn test_error_message_between_log_and_caller_caps_reaches_caller_intact() {
            let (client, mut server) = fake_lsp_client();

            let request_task = tokio::spawn(async move {
                client
                    .request::<_, Value>(
                        "textDocument/hover",
                        serde_json::json!({}),
                        Duration::from_secs(30),
                    )
                    .await
            });

            let mut reader = BufReader::new(&mut server.write_stdout);
            let request = read_framed_message(&mut reader).await;
            let id = request["id"].clone();
            let message = "x".repeat(MAX_ERROR_MESSAGE_LOG_BYTES + 50);
            write_error_response(&mut server.read_half_stdin, &id, -32603, &message).await;

            let result = request_task.await.unwrap();

            match result {
                Err(Error::LspServerError {
                    message: returned, ..
                }) => {
                    assert_eq!(
                        returned, message,
                        "message under the caller cap must not be truncated"
                    );
                }
                other => panic!("expected untruncated LspServerError, got {other:?}"),
            }
        }
    }
}
