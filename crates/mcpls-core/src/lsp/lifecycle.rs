//! LSP server lifecycle management.
//!
//! This module handles the complete lifecycle of an LSP server:
//! 1. Spawn server process
//! 2. Initialize → initialized handshake
//! 3. Capability negotiation
//! 4. Active request handling
//! 5. Graceful shutdown sequence

use std::collections::HashMap;
use std::env;
use std::path::{Path, PathBuf};
use std::process::Stdio;

#[cfg(unix)]
use std::os::unix::process::CommandExt;

use lsp_types::{
    ClientCapabilities, ClientInfo, GeneralClientCapabilities, InitializeParams, InitializeResult,
    InitializedParams, PositionEncodingKind, ServerCapabilities, WorkspaceFolder,
};
#[cfg(unix)]
use rustix::process::{Pid, Signal, kill_process_group, test_kill_process_group};
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::time::Duration;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::bridge::try_path_to_uri;
use crate::config::{LspServerConfig, ServerId};
use crate::error::{Error, Result, ServerSpawnFailure};
use crate::lsp::client::LspClient;
use crate::lsp::notification::non_blocking_notification_channel;
use crate::lsp::transport::LspTransport;
use crate::lsp::types::LspNotification;

/// Environment variables passed through to a spawned LSP server even though
/// its environment is otherwise cleared.
///
/// `PATH` lets the server resolve its own toolchain (e.g. rustup shims, venv
/// binaries); `HOME`/`USERPROFILE` and `TMPDIR`/`TEMP`/`TMP` let it find user
/// config/cache and scratch directories.
///
/// This list is not exhaustive: session-specific values that cannot be
/// hardcoded into a static [`LspServerConfig::env`] table (e.g.
/// `SSH_AUTH_SOCK`, which changes every login session) have no way through
/// today. See [`LspServerConfig::env`] for the config-level override/addition
/// mechanism this list feeds into.
const ENV_PASSTHROUGH: &[&str] = &["PATH", "HOME", "USERPROFILE", "TMPDIR", "TEMP", "TMP"];

/// Upper bound [`LspServer::shutdown`] waits for the child process to exit on
/// its own after sending the LSP `exit` notification, before falling back to
/// forced process-tree termination.
const CHILD_EXIT_GRACE: Duration = Duration::from_secs(3);

/// Bound project-environment discovery so a stuck `.envrc` cannot hold an
/// activation mailbox indefinitely.
const PROJECT_ENVIRONMENT_TIMEOUT: Duration = Duration::from_secs(5);

#[cfg(unix)]
#[derive(Debug, Clone, Copy)]
struct ProcessGroup(Pid);

#[cfg(unix)]
impl ProcessGroup {
    fn for_child(child: &tokio::process::Child) -> Option<Self> {
        child
            .id()
            .and_then(|pid| i32::try_from(pid).ok())
            .and_then(Pid::from_raw)
            .map(Self)
    }

    fn kill(self) {
        match kill_process_group(self.0, Signal::KILL) {
            Ok(()) | Err(rustix::io::Errno::SRCH) => {}
            Err(error) => warn!(%error, "failed to kill LSP server process group"),
        }
    }

    async fn terminate(self) {
        self.kill();
        let deadline = tokio::time::Instant::now() + CHILD_EXIT_GRACE;
        while test_kill_process_group(self.0).is_ok() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

struct LspProcess {
    child: tokio::process::Child,
    #[cfg(unix)]
    process_group: Option<ProcessGroup>,
}

impl LspProcess {
    fn isolated(child: tokio::process::Child) -> Self {
        #[cfg(unix)]
        let process_group = ProcessGroup::for_child(&child);
        Self {
            child,
            #[cfg(unix)]
            process_group,
        }
    }

    #[cfg(test)]
    const fn unisolated(child: tokio::process::Child) -> Self {
        Self {
            child,
            #[cfg(unix)]
            process_group: None,
        }
    }

    #[cfg(test)]
    fn id(&self) -> Option<u32> {
        self.child.id()
    }

    fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        self.child.try_wait()
    }

    async fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        self.child.wait().await
    }

    async fn terminate(&mut self) {
        #[cfg(unix)]
        if let Some(process_group) = self.process_group {
            let ((), wait_result) = tokio::join!(process_group.terminate(), self.child.wait());
            if let Err(error) = wait_result {
                warn!(%error, "failed to reap killed LSP server process");
            }
            return;
        }
        if let Err(error) = self.child.kill().await {
            warn!(%error, "failed to kill and reap LSP server process");
        }
    }

    #[cfg(unix)]
    const fn process_group(&self) -> Option<ProcessGroup> {
        self.process_group
    }
}

impl Drop for LspProcess {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(process_group) = self.process_group {
            process_group.kill();
        }
    }
}

/// Windows-only additions to [`ENV_PASSTHROUGH`].
///
/// `SystemRoot`/`SystemDrive`/`windir` are required by the Windows process
/// loader itself; `APPDATA`/`LOCALAPPDATA` are read by the Node-based default
/// servers (pyright, typescript-language-server) for global config and
/// cache; the rest are conventionally expected by Windows child processes.
#[cfg(windows)]
const ENV_PASSTHROUGH_WINDOWS: &[&str] = &[
    "SystemRoot",
    "SystemDrive",
    "windir",
    "APPDATA",
    "LOCALAPPDATA",
    "ProgramData",
    "ProgramFiles",
    "COMSPEC",
    "PATHEXT",
    "NUMBER_OF_PROCESSORS",
    "USERNAME",
];

/// Apply allowlisted values from a project's effective environment.
///
/// Explicit server configuration wins because it is the most specific source.
/// All other project variables are intentionally ignored so evaluating an
/// `.envrc` cannot leak arbitrary secrets into language-server processes.
pub fn apply_project_environment(
    config: &mut LspServerConfig,
    project_environment: &HashMap<String, Option<String>>,
) {
    for key in ENV_PASSTHROUGH {
        if let Some(Some(value)) = project_environment.get(*key) {
            config
                .env
                .entry((*key).to_string())
                .or_insert_with(|| value.clone());
        }
    }
    #[cfg(windows)]
    for key in ENV_PASSTHROUGH_WINDOWS {
        if let Some(Some(value)) = project_environment.get(*key) {
            config
                .env
                .entry((*key).to_string())
                .or_insert_with(|| value.clone());
        }
    }
}

/// State of an LSP server connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerState {
    /// Server has not been initialized.
    Uninitialized,
    /// Server is currently initializing.
    Initializing,
    /// Server is ready to handle requests.
    Ready,
    /// Server is shutting down.
    ShuttingDown,
    /// Server has been shut down.
    Shutdown,
}

impl ServerState {
    /// Check if the server is ready to handle requests.
    #[must_use]
    pub const fn is_ready(&self) -> bool {
        matches!(self, Self::Ready)
    }

    /// Check if the server can accept new requests.
    #[must_use]
    pub const fn can_accept_requests(&self) -> bool {
        matches!(self, Self::Ready)
    }
}

/// Configuration for LSP server initialization.
#[derive(Debug, Clone)]
pub struct ServerInitConfig {
    /// LSP server configuration.
    pub server_config: LspServerConfig,
    /// Workspace root paths.
    pub workspace_roots: Vec<PathBuf>,
    /// Initialization options (server-specific JSON).
    pub initialization_options: Option<serde_json::Value>,
    /// Position encoding preference order from
    /// [`crate::config::WorkspaceConfig::position_encodings`].
    ///
    /// Sent as `capabilities.general.positionEncodings` during [`LspServer::spawn`]'s
    /// `initialize` handshake, in the configured order. Values that don't parse
    /// as a valid [`PositionEncodingKind`] are skipped with a warning rather than
    /// failing the handshake: `serve`/`serve_with` validate the top-level
    /// `ServerConfig` via [`crate::config::ServerConfig::validate`] before this
    /// is ever built, but `LspServer::spawn`/`spawn_batch` are `pub` and
    /// reachable directly by a library embedder bypassing that validation
    /// entirely (same reasoning as the `initialize` timeout clamp below), so
    /// this can't assume the value was already checked. If nothing parses,
    /// falls back to `config::default_position_encodings()`'s default.
    pub position_encodings: Vec<String>,
    /// Optional channel for forwarding LSP notifications to the notification cache.
    ///
    /// When `Some`, the spawned LSP client sends every notification it receives
    /// (publishDiagnostics, logMessage, showMessage, …) through this sender.
    /// The caller is responsible for draining the corresponding receiver and
    /// storing entries in [`crate::bridge::NotificationCache`].
    pub notification_tx: Option<mpsc::Sender<LspNotification>>,
}

fn mcpls_workspace_edit_capabilities() -> lsp_types::WorkspaceEditClientCapabilities {
    lsp_types::WorkspaceEditClientCapabilities {
        document_changes: Some(true),
        resource_operations: Some(vec![
            lsp_types::ResourceOperationKind::Create,
            lsp_types::ResourceOperationKind::Rename,
            lsp_types::ResourceOperationKind::Delete,
        ]),
        ..Default::default()
    }
}

fn mcpls_file_operations_capabilities() -> lsp_types::WorkspaceFileOperationsClientCapabilities {
    lsp_types::WorkspaceFileOperationsClientCapabilities {
        will_rename: Some(true),
        ..Default::default()
    }
}

/// Result of attempting to spawn multiple LSP servers.
///
/// This type enables graceful degradation by collecting both
/// successful initializations and failures. Use the helper methods
/// to inspect the outcome and make decisions about how to proceed.
///
/// # Examples
///
/// ```
/// use mcpls_core::lsp::ServerInitResult;
/// use mcpls_core::error::ServerSpawnFailure;
///
/// let mut result = ServerInitResult::new();
///
/// // Check for different scenarios
/// if result.all_failed() {
///     eprintln!("All servers failed to initialize");
/// } else if result.partial_success() {
///     println!("Some servers succeeded, some failed");
/// } else if result.has_servers() {
///     println!("All servers initialized successfully");
/// }
/// ```
#[derive(Debug)]
pub struct ServerInitResult {
    /// Successfully initialized servers, keyed by routing identity.
    pub servers: HashMap<ServerId, LspServer>,
    /// Failures that occurred during spawn attempts.
    pub failures: Vec<ServerSpawnFailure>,
}

impl ServerInitResult {
    /// Create a new empty result.
    #[must_use]
    pub fn new() -> Self {
        Self {
            servers: HashMap::new(),
            failures: Vec::new(),
        }
    }

    /// Check if any servers were successfully initialized.
    ///
    /// Returns `true` if at least one server is available for use.
    #[must_use]
    pub fn has_servers(&self) -> bool {
        !self.servers.is_empty()
    }

    /// Check if all attempted servers failed.
    ///
    /// Returns `true` only if there were failures and no servers succeeded.
    /// Returns `false` for empty results (no servers configured).
    #[must_use]
    pub fn all_failed(&self) -> bool {
        self.servers.is_empty() && !self.failures.is_empty()
    }

    /// Check if some but not all servers failed.
    ///
    /// Returns `true` if there are both successful servers and failures.
    #[must_use]
    pub fn partial_success(&self) -> bool {
        !self.servers.is_empty() && !self.failures.is_empty()
    }

    /// Get the number of successfully initialized servers.
    #[must_use]
    pub fn server_count(&self) -> usize {
        self.servers.len()
    }

    /// Get the number of failures.
    #[must_use]
    pub const fn failure_count(&self) -> usize {
        self.failures.len()
    }

    /// Add a successful server.
    ///
    /// If a server with the same [`ServerId`] already exists, it will be replaced.
    pub fn add_server(&mut self, id: impl Into<ServerId>, server: LspServer) {
        self.servers.insert(id.into(), server);
    }

    /// Add a failure.
    pub fn add_failure(&mut self, failure: ServerSpawnFailure) {
        self.failures.push(failure);
    }
}

impl Default for ServerInitResult {
    fn default() -> Self {
        Self::new()
    }
}

/// Managed LSP server instance with capabilities and encoding.
pub struct LspServer {
    client: LspClient,
    capabilities: ServerCapabilities,
    server_name: Option<String>,
    position_encoding: PositionEncodingKind,
    workspace_roots: Vec<PathBuf>,
    /// Receiver for push notifications from the LSP server.
    ///
    /// Extract this before registering the server to receive real-time
    /// notifications (e.g., `textDocument/publishDiagnostics`, `$/progress`).
    pub notification_rx: mpsc::Receiver<LspNotification>,
    /// Language-server process tree, kept alive for crash detection and
    /// terminated as one unit during shutdown or failed initialization.
    process: LspProcess,
}

impl std::fmt::Debug for LspServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut debug = f.debug_struct("LspServer");
        debug
            .field("client", &self.client)
            .field("capabilities", &self.capabilities)
            .field("server_name", &self.server_name)
            .field("position_encoding", &self.position_encoding)
            .field("workspace_roots", &self.workspace_roots)
            .field("notification_rx", &"<channel>")
            .field("process", &"<process tree>");
        debug.finish()
    }
}

impl LspServer {
    /// Take the notification receiver out of this server, replacing it with a dummy channel.
    ///
    /// Use this to extract the receiver for a background pump task before registering
    /// the server with the translator. After this call, the server's `notification_rx`
    /// will never receive messages.
    pub fn take_notification_rx(&mut self) -> tokio::sync::mpsc::Receiver<LspNotification> {
        let (_, dummy) = tokio::sync::mpsc::channel(1);
        std::mem::replace(&mut self.notification_rx, dummy)
    }

    /// Take the native filesystem-watcher stream out of this server.
    ///
    /// The project owns the shared document tracker, so it consumes these
    /// paths and invalidates tracked snapshots before the next semantic call.
    pub fn take_watch_change_rx(&mut self) -> mpsc::UnboundedReceiver<PathBuf> {
        self.client.take_watch_change_rx()
    }

    /// Spawn and initialize LSP server.
    ///
    /// This performs the complete initialization sequence:
    /// 1. Spawns the LSP server as a child process
    /// 2. Sends initialize request with client capabilities
    /// 3. Receives server capabilities from initialize response
    /// 4. Sends initialized notification
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - Server process fails to spawn
    /// - Initialize request fails or times out
    /// - Server returns error during initialization
    pub async fn spawn(config: ServerInitConfig) -> Result<Self> {
        Self::spawn_with_cancellation(config, CancellationToken::new()).await
    }

    /// Spawn and initialize an LSP server, stopping and reaping it when the
    /// caller cancels initialization.
    ///
    /// # Errors
    ///
    /// Returns an error if the server cannot spawn, initialize, or is
    /// cancelled before initialization completes.
    pub async fn spawn_with_cancellation(
        config: ServerInitConfig,
        cancellation: CancellationToken,
    ) -> Result<Self> {
        info!(
            "Spawning LSP server: {} {:?}",
            config.server_config.command, config.server_config.args
        );

        let mut command = Self::build_command(&config.server_config, |key| std::env::var_os(key));
        if let Some(root) = config.workspace_roots.first() {
            command.current_dir(root);
        }

        // Log allowlist presence and an override count only — never the
        // configured keys themselves, since `config.server_config.env` may
        // hold secret-bearing names (e.g. `AWS_SECRET_ACCESS_KEY`) whose
        // mere presence in a debug log would be its own disclosure.
        let passthrough_present = {
            let base = ENV_PASSTHROUGH
                .iter()
                .filter(|key| std::env::var_os(key).is_some())
                .count();
            #[cfg(windows)]
            let windows = ENV_PASSTHROUGH_WINDOWS
                .iter()
                .filter(|key| std::env::var_os(key).is_some())
                .count();
            #[cfg(not(windows))]
            let windows = 0;
            base + windows
        };
        debug!(
            "Effective LSP server env: {passthrough_present} allowlisted key(s) present, \
             {} configured override(s) applied",
            config.server_config.env.len()
        );

        let child = command.spawn().map_err(|e| Error::ServerSpawnFailed {
            command: config.server_config.command.clone(),
            source: e,
        })?;
        let mut process = LspProcess::isolated(child);

        if cancellation.is_cancelled() {
            process.terminate().await;
            return Err(Error::LspInitFailed {
                message: "LSP initialization cancelled".to_string(),
            });
        }

        let stdin = process
            .child
            .stdin
            .take()
            .ok_or_else(|| Error::Transport("Failed to capture stdin".to_string()))?;
        let stdout = process
            .child
            .stdout
            .take()
            .ok_or_else(|| Error::Transport("Failed to capture stdout".to_string()))?;

        let transport = LspTransport::new(stdin, stdout);
        let (notification_sink, notification_rx) = non_blocking_notification_channel(64);
        let client = LspClient::from_transport_with_notification_sink(
            config.server_config.clone(),
            transport,
            notification_sink,
            config.workspace_roots.clone(),
        );

        let (capabilities, position_encoding, server_name) = match tokio::select! {
            result = Self::initialize(&client, &config) => result,
            () = cancellation.cancelled() => {
                process.terminate().await;
                return Err(Error::LspInitFailed {
                    message: "LSP initialization cancelled".to_string(),
                });
            }
        } {
            Ok(initialized) => initialized,
            Err(error) => {
                process.terminate().await;
                return Err(error);
            }
        };

        info!("LSP server initialized successfully");

        Ok(Self {
            client,
            capabilities,
            server_name,
            position_encoding,
            workspace_roots: config.workspace_roots,
            notification_rx,
            process,
        })
    }

    /// Build the child `Command` for a spawned LSP server, without spawning it.
    ///
    /// The child's environment is cleared, then [`ENV_PASSTHROUGH`] (plus
    /// [`ENV_PASSTHROUGH_WINDOWS`] under `cfg(windows)`) is copied in from
    /// `parent_env` for whichever of those keys it returns `Some` for, then
    /// `config.env` is applied last so it can override any passthrough
    /// value. `parent_env` is injected (production passes
    /// `std::env::var_os`) so tests can supply a fixed environment without
    /// racing on real process-global state.
    fn build_command(
        config: &LspServerConfig,
        parent_env: impl Fn(&str) -> Option<std::ffi::OsString>,
    ) -> Command {
        let mut command = Command::new(&config.command);
        command.args(&config.args).env_clear();

        for key in ENV_PASSTHROUGH {
            if let Some(value) = parent_env(key) {
                command.env(key, value);
            }
        }
        #[cfg(windows)]
        for key in ENV_PASSTHROUGH_WINDOWS {
            if let Some(value) = parent_env(key) {
                command.env(key, value);
            }
        }

        command
            .envs(&config.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        #[cfg(unix)]
        command.as_std_mut().process_group(0);

        command
    }

    /// Perform LSP initialization handshake.
    ///
    /// Sends initialize request and waits for response, then sends initialized notification.
    #[allow(clippy::too_many_lines)]
    async fn initialize(
        client: &LspClient,
        config: &ServerInitConfig,
    ) -> Result<(ServerCapabilities, PositionEncodingKind, Option<String>)> {
        debug!("Sending initialize request");

        let workspace_folders: Vec<WorkspaceFolder> = config
            .workspace_roots
            .iter()
            .map(|root| workspace_folder(root))
            .collect::<Result<Vec<_>>>()?;

        let params = InitializeParams {
            process_id: Some(std::process::id()),
            #[allow(deprecated)]
            root_uri: None,
            initialization_options: config.initialization_options.clone(),
            capabilities: ClientCapabilities {
                experimental: Some(serde_json::json!({
                    "serverStatusNotification": true,
                })),
                general: Some(GeneralClientCapabilities {
                    position_encodings: Some(resolve_position_encodings(
                        &config.position_encodings,
                    )),
                    ..Default::default()
                }),
                text_document: Some(lsp_types::TextDocumentClientCapabilities {
                    hover: Some(lsp_types::HoverClientCapabilities {
                        dynamic_registration: Some(false),
                        content_format: Some(vec![
                            lsp_types::MarkupKind::Markdown,
                            lsp_types::MarkupKind::PlainText,
                        ]),
                    }),
                    definition: Some(lsp_types::GotoCapability {
                        dynamic_registration: Some(false),
                        link_support: Some(true),
                    }),
                    declaration: Some(lsp_types::GotoCapability {
                        dynamic_registration: Some(false),
                        link_support: Some(true),
                    }),
                    selection_range: Some(lsp_types::SelectionRangeClientCapabilities {
                        dynamic_registration: Some(false),
                    }),
                    references: Some(lsp_types::ReferenceClientCapabilities {
                        dynamic_registration: Some(false),
                    }),
                    code_action: Some(lsp_types::CodeActionClientCapabilities {
                        dynamic_registration: Some(false),
                        data_support: Some(true),
                        resolve_support: Some(lsp_types::CodeActionCapabilityResolveSupport {
                            properties: vec!["edit".to_string()],
                        }),
                        // Declare supported action kinds so the server returns
                        // CodeAction objects (not just legacy Command objects).
                        code_action_literal_support: Some(lsp_types::CodeActionLiteralSupport {
                            code_action_kind: lsp_types::CodeActionKindLiteralSupport {
                                value_set: [
                                    lsp_types::CodeActionKind::EMPTY,
                                    lsp_types::CodeActionKind::QUICKFIX,
                                    lsp_types::CodeActionKind::REFACTOR,
                                    lsp_types::CodeActionKind::REFACTOR_EXTRACT,
                                    lsp_types::CodeActionKind::REFACTOR_INLINE,
                                    lsp_types::CodeActionKind::REFACTOR_REWRITE,
                                    lsp_types::CodeActionKind::SOURCE,
                                    lsp_types::CodeActionKind::SOURCE_ORGANIZE_IMPORTS,
                                ]
                                .iter()
                                .map(|kind| kind.as_str().to_string())
                                .collect(),
                            },
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                workspace: Some(lsp_types::WorkspaceClientCapabilities {
                    workspace_edit: Some(mcpls_workspace_edit_capabilities()),
                    file_operations: Some(mcpls_file_operations_capabilities()),
                    workspace_folders: Some(true),
                    did_change_watched_files: Some(
                        lsp_types::DidChangeWatchedFilesClientCapabilities {
                            dynamic_registration: Some(true),
                            relative_pattern_support: Some(true),
                        },
                    ),
                    ..Default::default()
                }),
                ..Default::default()
            },
            client_info: Some(ClientInfo {
                name: "mcpls".to_string(),
                version: Some(env!("CARGO_PKG_VERSION").to_string()),
            }),
            workspace_folders: Some(workspace_folders),
            ..Default::default()
        };

        // Use the server's configured timeout for the initialize handshake too,
        // not a hardcoded 30s: large solutions (e.g. a 130-project Unity .sln via
        // OmniSharp) take minutes to respond to `initialize`.
        let result: InitializeResult = client
            .request(
                "initialize",
                params,
                // Clamped for the same reason as `LspClient::request_timeout`:
                // `serve()`/`serve_with()` now validate the top-level
                // `ServerConfig` via `ServerConfig::validate()`, but this call
                // operates on the per-server `config.server_config` reached
                // through `LspServer::spawn`/`spawn_batch`, which bypass that
                // top-level validation entirely, so an out-of-range value (0,
                // or an unbounded one that would silently disable the timeout
                // via tokio's `Instant::far_future()` fallback) is still
                // reachable here and needs a last-line-of-defense clamp.
                Duration::from_secs(
                    config
                        .server_config
                        .timeout_seconds
                        .clamp(1, crate::config::MAX_TIMEOUT_SECONDS),
                ),
            )
            .await
            .map_err(|e| Error::LspInitFailed {
                message: format!("Initialize request failed: {e}"),
            })?;

        let position_encoding = result
            .capabilities
            .position_encoding
            .clone()
            .unwrap_or(PositionEncodingKind::UTF16);
        let server_name = result.server_info.as_ref().map(|info| info.name.clone());

        debug!(
            "Server capabilities received, encoding: {:?}",
            position_encoding
        );

        client
            .notify("initialized", InitializedParams {})
            .await
            .map_err(|e| Error::LspInitFailed {
                message: format!("Initialized notification failed: {e}"),
            })?;

        client.set_ready().await;

        Ok((result.capabilities, position_encoding, server_name))
    }

    /// Get server capabilities.
    #[must_use]
    pub const fn capabilities(&self) -> &ServerCapabilities {
        &self.capabilities
    }

    /// Whether the initialized server identifies itself as rust-analyzer.
    #[must_use]
    pub fn is_rust_analyzer(&self) -> bool {
        self.server_name
            .as_deref()
            .is_some_and(|name| name.eq_ignore_ascii_case("rust-analyzer"))
    }

    /// Get negotiated position encoding.
    #[must_use]
    pub fn position_encoding(&self) -> PositionEncodingKind {
        self.position_encoding.clone()
    }

    /// Get workspace roots used to initialize this server.
    #[must_use]
    pub fn workspace_roots(&self) -> &[PathBuf] {
        &self.workspace_roots
    }

    /// Get client for making requests.
    #[must_use]
    pub const fn client(&self) -> &LspClient {
        &self.client
    }

    /// Non-blocking check for whether the child process has already exited.
    ///
    /// Uses [`tokio::process::Child::try_wait`], which never blocks waiting
    /// for the process: `true` means it is gone (crashed, killed, or exited
    /// on its own), and any [`LspClient`] obtained from [`Self::client`] is
    /// now permanently disconnected -- new requests through it fail with
    /// [`crate::error::Error::ServerTerminated`]. Callers that want to
    /// recover substitute a freshly [`Self::spawn`]ed replacement.
    ///
    /// # Errors
    ///
    /// Returns an error if the OS fails to report the process's status.
    pub fn has_exited(&mut self) -> Result<bool> {
        Ok(self.process.try_wait()?.is_some())
    }

    /// Shutdown server gracefully.
    ///
    /// Sends the LSP `shutdown` request, waits for the response, sends the
    /// `exit` notification, then waits up to a fixed grace period for the
    /// child process to exit on its own. If it hasn't by then, or if the
    /// `shutdown`/`exit` handshake itself fails, the isolated process group is
    /// killed and the server parent is awaited before this method returns.
    ///
    /// # Errors
    ///
    /// Returns an error if the `shutdown`/`exit` handshake fails. The child
    /// process is still torn down (gracefully if it exits in time, killed
    /// otherwise) regardless of whether this returns `Ok` or `Err`.
    pub async fn shutdown(self) -> Result<()> {
        debug!("Shutting down LSP server");

        let handshake: Result<()> = async move {
            let _: serde_json::Value = self
                .client
                .request("shutdown", serde_json::Value::Null, Duration::from_secs(5))
                .await?;
            self.client.notify("exit", serde_json::Value::Null).await?;
            match self.client.shutdown().await {
                // A server is expected to close its transport after `exit`.
                // The receiver can observe that EOF before the client-side
                // shutdown command is processed, so treat that orderly
                // post-exit termination as a successful handshake.
                Err(Error::ServerTerminated) => Ok(()),
                result => result,
            }
        }
        .await;

        let mut process = self.process;
        let child_reaped = match tokio::time::timeout(CHILD_EXIT_GRACE, process.wait()).await {
            Ok(Ok(status)) => {
                debug!(
                    ?status,
                    "LSP server process exited after `exit` notification"
                );
                true
            }
            Ok(Err(e)) => {
                warn!(error = %e, "failed to wait for LSP server process exit");
                false
            }
            Err(_) => {
                warn!(
                    timeout = ?CHILD_EXIT_GRACE,
                    "LSP server process did not exit within grace period after `exit` \
                     notification, killing it"
                );
                false
            }
        };
        if child_reaped {
            #[cfg(unix)]
            if let Some(process_group) = process.process_group() {
                process_group.terminate().await;
            }
        } else {
            #[cfg(unix)]
            let wait_result = if let Some(process_group) = process.process_group() {
                let ((), wait_result) = tokio::join!(process_group.terminate(), process.wait());
                wait_result
            } else {
                process.wait().await
            };
            #[cfg(not(unix))]
            let wait_result = process.wait().await;
            if let Err(error) = wait_result {
                warn!(%error, "failed to reap killed LSP server process");
            }
        }

        handshake?;
        info!("LSP server shut down successfully");
        Ok(())
    }

    /// Spawn multiple LSP servers in batch mode with graceful degradation.
    ///
    /// Attempts to spawn and initialize all configured servers. If some servers
    /// fail to spawn, the successful servers are still returned. This enables
    /// graceful degradation where the system can continue to operate with
    /// partial functionality.
    ///
    /// # Behavior
    ///
    /// - Attempts to spawn all servers concurrently
    /// - Logs success (info) and failure (error) for each server
    /// - Accumulates successful servers and failures
    /// - Never panics or returns early - attempts all servers
    ///
    /// # Examples
    ///
    /// ```
    /// use mcpls_core::lsp::{LspServer, ServerInitConfig};
    /// use mcpls_core::config::LspServerConfig;
    /// use std::path::PathBuf;
    ///
    /// # async fn example() {
    /// let configs = vec![
    ///     ServerInitConfig {
    ///         server_config: LspServerConfig::rust_analyzer(),
    ///         workspace_roots: vec![PathBuf::from("/workspace")],
    ///         initialization_options: None,
    ///         position_encodings: vec!["utf-8".to_string(), "utf-16".to_string()],
    ///         notification_tx: None,
    ///     },
    ///     ServerInitConfig {
    ///         server_config: LspServerConfig::pyright(),
    ///         workspace_roots: vec![PathBuf::from("/workspace")],
    ///         initialization_options: None,
    ///         position_encodings: vec!["utf-8".to_string(), "utf-16".to_string()],
    ///         notification_tx: None,
    ///     },
    /// ];
    ///
    /// let result = LspServer::spawn_batch(&configs).await;
    ///
    /// if result.has_servers() {
    ///     println!("Successfully spawned {} servers", result.server_count());
    /// }
    ///
    /// if result.partial_success() {
    ///     eprintln!("Warning: {} servers failed", result.failure_count());
    /// }
    /// # }
    /// ```
    pub async fn spawn_batch(configs: &[ServerInitConfig]) -> ServerInitResult {
        Self::spawn_batch_with_cancellation(configs, CancellationToken::new()).await
    }

    /// Spawn multiple LSP servers while honoring a caller-owned cancellation
    /// signal and reaping every server that initialized before cancellation.
    pub async fn spawn_batch_with_cancellation(
        configs: &[ServerInitConfig],
        cancellation: CancellationToken,
    ) -> ServerInitResult {
        let mut result = ServerInitResult::new();
        let attempts = configs.iter().cloned().map(|config| {
            let cancellation = cancellation.clone();
            async move {
                let server_id = config.server_config.id();
                let language_id = config.server_config.language_id.clone();
                let command = config.server_config.command.clone();
                let outcome = Self::spawn_with_cancellation(config, cancellation).await;
                (server_id, language_id, command, outcome)
            }
        });

        for (server_id, language_id, command, outcome) in futures::future::join_all(attempts).await
        {
            match outcome {
                Ok(server) => {
                    info!(
                        "Successfully spawned LSP server: {} ({})",
                        server_id, command
                    );
                    result.add_server(server_id, server);
                }
                Err(e) => {
                    tracing::error!(
                        "Failed to spawn LSP server: {} ({}): {}",
                        server_id,
                        command,
                        e
                    );
                    result.add_failure(ServerSpawnFailure {
                        server_id,
                        language_id,
                        command,
                        message: e.to_string(),
                    });
                }
            }
        }

        if cancellation.is_cancelled() {
            let servers = std::mem::take(&mut result.servers);
            for server in servers.into_values() {
                let _ = server.shutdown().await;
            }
        }

        result
    }
}

/// Convert configured position-encoding strings into the ordered
/// [`PositionEncodingKind`] list offered during the `initialize` handshake.
///
/// Values that don't parse are skipped with a warning instead of failing the
/// handshake (see [`ServerInitConfig::position_encodings`] for why this can't
/// assume [`crate::config::ServerConfig::validate`] already ran). Falls back
/// to `config::default_position_encodings()` -- the same default used when
/// nothing is configured at all -- if no configured value parses.
fn resolve_position_encodings(configured: &[String]) -> Vec<PositionEncodingKind> {
    let encodings: Vec<PositionEncodingKind> = configured
        .iter()
        .filter_map(|value| {
            let kind = crate::config::parse_position_encoding(value);
            if kind.is_none() {
                warn!(value = %value, "ignoring invalid configured position encoding");
            }
            kind
        })
        .collect();

    if encodings.is_empty() {
        crate::config::default_position_encodings()
            .iter()
            .filter_map(|value| crate::config::parse_position_encoding(value))
            .collect()
    } else {
        encodings
    }
}

/// Build the `workspace/workspaceFolders` entry for one configured root.
///
/// Reserved characters have to be percent-encoded here: an unencoded `#`
/// would truncate the path into a URI fragment, and `[` / `]` are rejected
/// outright by `Uri`.
fn workspace_folder(root: &Path) -> Result<WorkspaceFolder> {
    let uri = try_path_to_uri(root).ok_or_else(|| {
        let root_display = root.display();
        Error::InvalidUri(format!("Invalid workspace root: {root_display}"))
    })?;
    Ok(WorkspaceFolder {
        uri,
        name: root
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("workspace")
            .to_string(),
    })
}

/// Load the effective process environment for one project root.
pub async fn load_project_environment(root: &Path) -> Option<HashMap<String, Option<String>>> {
    if root.join(".envrc").is_file()
        && let Some(root_string) = root.to_str()
        && let Some(environment) =
            command_environment("direnv", ["exec", root_string, "env"], root).await
    {
        info!("Loaded LSP environment from direnv: {}", root.display());
        return Some(environment);
    }

    let root_string = root.to_str()?;
    if root.join("flake.nix").is_file()
        && let Some(environment) =
            command_environment("nix", ["develop", root_string, "-c", "env"], root).await
    {
        info!(
            "Loaded LSP environment from nix develop: {}",
            root.display()
        );
        return Some(environment);
    }

    None
}

async fn command_environment<I, S>(
    command: &str,
    args: I,
    root: &Path,
) -> Option<HashMap<String, Option<String>>>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    command_environment_with_timeout(command, args, root, PROJECT_ENVIRONMENT_TIMEOUT).await
}

async fn command_environment_with_timeout<I, S>(
    command: &str,
    args: I,
    root: &Path,
    timeout: Duration,
) -> Option<HashMap<String, Option<String>>>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let output = tokio::time::timeout(
        timeout,
        Command::new(command)
            .args(args)
            .current_dir(root)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| {
        warn!(command, ?timeout, root = %root.display(), "project environment command timed out");
    })
    .ok()?
    .ok()?;
    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8(output.stdout).ok()?;
    Some(
        stdout
            .lines()
            .filter_map(|line| {
                let (key, value) = line.split_once('=')?;
                Some((key.to_string(), Some(value.to_string())))
            })
            .collect(),
    )
}

/// Resolve a configured command against the daemon and project environments.
pub fn resolve_command(
    command: &str,
    project_env: Option<&HashMap<String, Option<String>>>,
) -> PathBuf {
    let command_path = PathBuf::from(command);
    if command_path.is_absolute() || command_path.components().count() > 1 {
        return command_path;
    }

    let current_path = env::var("PATH").ok();
    let project_path = project_env
        .and_then(|environment| environment.get("PATH"))
        .and_then(Option::as_deref);
    for path in [current_path.as_deref(), project_path]
        .into_iter()
        .flatten()
    {
        for directory in env::split_paths(path) {
            let candidate = directory.join(command);
            if candidate.is_file() {
                return candidate;
            }
        }
    }
    command_path
}

/// Builds an `LspServer` backed by mock `echo`/`cat` child processes, so it
/// can be registered without a real language server.
///
/// `pub` rather than private to this module's own `tests` (`lifecycle` is a
/// private module, so this stays crate-scoped in practice, per the
/// `redundant_pub_crate` clippy lint): it constructs `LspServer` via a
/// struct literal, which only code inside this module can do (all its
/// fields are private), so this is the one place other modules'
/// shutdown-path tests (`bridge::translator`, `lib.rs`) can get a real,
/// registerable `LspServer` from.
#[cfg(test)]
#[allow(clippy::unwrap_used)]
pub fn fake_lsp_server() -> LspServer {
    let mock_child = tokio::process::Command::new("echo")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mock_stdin = tokio::process::Command::new("cat")
        .stdin(Stdio::piped())
        .spawn()
        .unwrap()
        .stdin
        .take()
        .unwrap();
    let mock_stdout = tokio::process::Command::new("echo")
        .stdout(Stdio::piped())
        .spawn()
        .unwrap()
        .stdout
        .take()
        .unwrap();
    let transport = LspTransport::new(mock_stdin, mock_stdout);
    let client = LspClient::from_transport(LspServerConfig::pyright(), transport);
    let (_, mock_notification_rx) = mpsc::channel(1);
    LspServer {
        client,
        capabilities: lsp_types::ServerCapabilities::default(),
        server_name: None,
        position_encoding: PositionEncodingKind::UTF8,
        workspace_roots: Vec::new(),
        notification_rx: mock_notification_rx,
        process: LspProcess::unisolated(mock_child),
    }
}

#[cfg(test)]
impl LspServer {
    /// Construct an `LspServer` fixture carrying the given capabilities, for
    /// tests elsewhere in the crate that need to drive capability-gated
    /// dispatch paths in `Translator` without spawning a real language server.
    ///
    /// The underlying client and child process are inert placeholders — only
    /// `capabilities()` is meaningful on the returned value.
    ///
    /// Uses `LspClient::new` (uninitialized, no background task) rather than
    /// `LspClient::from_transport`, so this does not depend on the Tokio
    /// message loop — only `child`'s spawn needs a Tokio runtime, i.e. an
    /// async test context (`#[tokio::test]`).
    #[allow(clippy::unwrap_used)]
    pub(crate) fn new_for_test(capabilities: ServerCapabilities) -> Self {
        Self::new_for_test_with_encoding(capabilities, PositionEncodingKind::UTF16)
    }

    /// As [`Self::new_for_test`], but with a caller-chosen negotiated
    /// encoding -- for tests exercising a non-UTF-16 conversion path (e.g.
    /// `EncodingCtx`-driven range conversion) without spawning a real
    /// process.
    #[allow(clippy::unwrap_used)]
    pub(crate) fn new_for_test_with_encoding(
        capabilities: ServerCapabilities,
        position_encoding: PositionEncodingKind,
    ) -> Self {
        #[cfg(unix)]
        let mut command = Command::new("sleep");
        #[cfg(unix)]
        command.arg("3600");
        #[cfg(windows)]
        let mut command = {
            let mut command = Command::new("cmd");
            command.args(["/C", "ping -n 3600 127.0.0.1 >NUL"]);
            command
        };
        let child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();

        let client = LspClient::new(LspServerConfig::rust_analyzer());
        let (_, notification_rx) = mpsc::channel(1);

        Self {
            client,
            capabilities,
            server_name: None,
            position_encoding,
            workspace_roots: Vec::new(),
            notification_rx,
            process: LspProcess::unisolated(child),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn client_capabilities_advertise_supported_workspace_edit_operations() {
        let workspace_edit = mcpls_workspace_edit_capabilities();

        assert_eq!(workspace_edit.document_changes, Some(true));
        assert_eq!(
            workspace_edit.resource_operations,
            Some(vec![
                lsp_types::ResourceOperationKind::Create,
                lsp_types::ResourceOperationKind::Rename,
                lsp_types::ResourceOperationKind::Delete,
            ])
        );
    }

    #[test]
    fn client_capabilities_advertise_will_rename_files() {
        let file_operations = mcpls_file_operations_capabilities();

        assert_eq!(file_operations.will_rename, Some(true));
    }

    #[cfg(unix)]
    fn write_coordinated_lsp(directory: &tempfile::TempDir) -> PathBuf {
        let source = directory.path().join("coordinated-lsp.rs");
        let command = directory.path().join("coordinated-lsp");
        std::fs::write(
            &source,
            r#"
use std::{env, fs, io::{self, Read, Write}, thread, time::Duration};

fn read_message() -> Option<String> {
    let mut headers = Vec::new();
    let mut byte = [0; 1];
    while !headers.ends_with(b"\r\n\r\n") {
        io::stdin().read_exact(&mut byte).ok()?;
        headers.push(byte[0]);
    }
    let headers = String::from_utf8(headers).ok()?;
    let length = headers.lines().find_map(|line| {
        line.strip_prefix("Content-Length:")?.trim().parse::<usize>().ok()
    })?;
    let mut body = vec![0; length];
    io::stdin().read_exact(&mut body).ok()?;
    String::from_utf8(body).ok()
}

fn request_id(body: &str) -> Option<&str> {
    let value = body[body.find("\"id\"")?..].split_once(':')?.1.trim_start();
    let end = value.find(|character: char| !character.is_ascii_digit()).unwrap_or(value.len());
    Some(&value[..end])
}

fn send(body: &str) {
    print!("Content-Length: {}\r\n\r\n{}", body.len(), body);
    io::stdout().flush().unwrap();
}

fn main() {
    while let Some(message) = read_message() {
        // Match the method name independent of JSON whitespace; this mock is
        // intentionally a protocol fixture, not a string-format fixture.
        if message.contains("initialize") && message.contains("\"id\"") {
            if let Some(path) = env::var_os("MCPLS_SIGNAL_FILE") {
                fs::write(path, "ready").unwrap();
            }
            if let Some(path) = env::var_os("MCPLS_WAIT_FILE") {
                while !std::path::Path::new(&path).exists() {
                    thread::sleep(Duration::from_millis(5));
                }
            }
            let id = request_id(&message).unwrap();
            send(&format!("{{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":{{\"capabilities\":{{\"positionEncoding\":\"utf-8\"}}}}}}"));
        } else if message.contains("shutdown") && message.contains("\"id\"") {
            let id = request_id(&message).unwrap();
            send(&format!("{{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":null}}"));
        } else if message.contains("exit") {
            break;
        }
    }
}
"#,
        )
        .unwrap();
        let status = std::process::Command::new("rustc")
            .args([
                "--edition=2021",
                source.to_str().unwrap(),
                "-o",
                command.to_str().unwrap(),
            ])
            .status()
            .unwrap();
        assert!(status.success(), "failed to compile coordinated mock LSP");
        command
    }

    #[test]
    fn test_resolve_position_encodings_preserves_configured_order() {
        let result = resolve_position_encodings(&["utf-32".to_string(), "utf-8".to_string()]);
        assert_eq!(
            result,
            vec![PositionEncodingKind::UTF32, PositionEncodingKind::UTF8]
        );
    }

    #[test]
    fn test_resolve_position_encodings_skips_invalid_and_keeps_valid() {
        let result = resolve_position_encodings(&["utf-7".to_string(), "utf-16".to_string()]);
        assert_eq!(result, vec![PositionEncodingKind::UTF16]);
    }

    #[test]
    fn test_resolve_position_encodings_falls_back_when_all_invalid() {
        let result = resolve_position_encodings(&["utf-7".to_string(), "bogus".to_string()]);
        assert_eq!(
            result,
            vec![PositionEncodingKind::UTF8, PositionEncodingKind::UTF16]
        );
    }

    #[test]
    fn test_resolve_position_encodings_falls_back_when_empty() {
        let result = resolve_position_encodings(&[]);
        assert_eq!(
            result,
            vec![PositionEncodingKind::UTF8, PositionEncodingKind::UTF16]
        );
    }

    #[test]
    fn test_server_state_ready() {
        assert!(ServerState::Ready.is_ready());
        assert!(ServerState::Ready.can_accept_requests());
    }

    #[test]
    fn test_server_state_uninitialized() {
        assert!(!ServerState::Uninitialized.is_ready());
        assert!(!ServerState::Uninitialized.can_accept_requests());
    }

    #[test]
    fn test_server_state_initializing() {
        assert!(!ServerState::Initializing.is_ready());
        assert!(!ServerState::Initializing.can_accept_requests());
    }

    #[test]
    fn test_workspace_folder_encodes_fragment_char() {
        // An unencoded `#` parses as a fragment, silently handing the server
        // the parent directory as its root.
        #[cfg(windows)]
        let (root, expected) = (
            Path::new(r"C:\home\me\dev\#work"),
            "file:///C:/home/me/dev/%23work",
        );
        #[cfg(not(windows))]
        let (root, expected) = (
            Path::new("/home/me/dev/#work"),
            "file:///home/me/dev/%23work",
        );

        let folder = workspace_folder(root).unwrap();

        assert_eq!(folder.uri.as_str(), expected);
        assert_eq!(folder.name, "#work");
    }

    #[test]
    fn test_workspace_folder_encodes_bracket_chars() {
        #[cfg(windows)]
        let (root, expected) = (
            Path::new(r"C:\home\me\dev\[env]"),
            "file:///C:/home/me/dev/%5Benv%5D",
        );
        #[cfg(not(windows))]
        let (root, expected) = (
            Path::new("/home/me/dev/[env]"),
            "file:///home/me/dev/%5Benv%5D",
        );

        let folder = workspace_folder(root).unwrap();

        assert_eq!(folder.uri.as_str(), expected);
        assert_eq!(folder.name, "[env]");
    }

    #[test]
    fn test_workspace_folder_rejects_relative_root() {
        let err = workspace_folder(Path::new("relative/root")).unwrap_err();
        assert!(matches!(err, Error::InvalidUri(_)), "got {err:?}");
    }

    #[test]
    fn test_server_state_shutting_down() {
        assert!(!ServerState::ShuttingDown.is_ready());
        assert!(!ServerState::ShuttingDown.can_accept_requests());
    }

    #[test]
    fn test_server_state_shutdown() {
        assert!(!ServerState::Shutdown.is_ready());
        assert!(!ServerState::Shutdown.can_accept_requests());
    }

    #[test]
    fn test_server_state_equality() {
        assert_eq!(ServerState::Ready, ServerState::Ready);
        assert_ne!(ServerState::Ready, ServerState::Uninitialized);
        assert_eq!(ServerState::Shutdown, ServerState::Shutdown);
    }

    #[test]
    fn test_server_state_clone() {
        let state = ServerState::Ready;
        let cloned = state;
        assert_eq!(state, cloned);
    }

    #[test]
    fn test_server_state_debug() {
        let state = ServerState::Ready;
        let debug_str = format!("{state:?}");
        assert!(debug_str.contains("Ready"));
    }

    #[test]
    fn test_server_init_config_clone() {
        let config = ServerInitConfig {
            server_config: LspServerConfig::rust_analyzer(),
            workspace_roots: vec![PathBuf::from("/tmp/workspace")],
            initialization_options: Some(serde_json::json!({"key": "value"})),
            position_encodings: vec!["utf-8".to_string(), "utf-16".to_string()],
            notification_tx: None,
        };

        #[allow(clippy::redundant_clone)]
        let cloned = config.clone();
        assert_eq!(cloned.server_config.language_id, "rust");
        assert_eq!(cloned.workspace_roots.len(), 1);
    }

    #[test]
    fn test_server_init_config_debug() {
        let config = ServerInitConfig {
            server_config: LspServerConfig::pyright(),
            workspace_roots: vec![],
            initialization_options: None,
            position_encodings: vec!["utf-8".to_string(), "utf-16".to_string()],
            notification_tx: None,
        };

        let debug_str = format!("{config:?}");
        assert!(debug_str.contains("python"));
        assert!(debug_str.contains("pyright"));
    }

    #[test]
    fn test_server_init_config_with_options() {
        use std::collections::HashMap;

        let init_opts = serde_json::json!({
            "settings": {
                "python": {
                    "analysis": {
                        "typeCheckingMode": "strict"
                    }
                }
            }
        });

        let mut env = HashMap::new();
        env.insert("PYTHONPATH".to_string(), "/usr/lib".to_string());

        let config = ServerInitConfig {
            server_config: LspServerConfig {
                language_id: "python".to_string(),
                command: "pyright-langserver".to_string(),
                args: vec!["--stdio".to_string()],
                env,
                file_patterns: vec!["**/*.py".to_string()],
                initialization_options: Some(init_opts.clone()),
                timeout_seconds: 10,
                request_timeout_seconds: 10,
                heuristics: None,
                name: None,
                handles: None,
            },
            workspace_roots: vec![PathBuf::from("/workspace")],
            initialization_options: Some(init_opts),
            position_encodings: vec!["utf-8".to_string(), "utf-16".to_string()],
            notification_tx: None,
        };

        assert!(config.initialization_options.is_some());
        assert_eq!(config.workspace_roots.len(), 1);
    }

    #[test]
    fn test_server_init_config_empty_workspace() {
        let config = ServerInitConfig {
            server_config: LspServerConfig::typescript(),
            workspace_roots: vec![],
            initialization_options: None,
            position_encodings: vec!["utf-8".to_string(), "utf-16".to_string()],
            notification_tx: None,
        };

        assert!(config.workspace_roots.is_empty());
    }

    #[test]
    fn test_server_init_config_multiple_workspaces() {
        let config = ServerInitConfig {
            server_config: LspServerConfig::rust_analyzer(),
            workspace_roots: vec![
                PathBuf::from("/workspace1"),
                PathBuf::from("/workspace2"),
                PathBuf::from("/workspace3"),
            ],
            initialization_options: None,
            position_encodings: vec!["utf-8".to_string(), "utf-16".to_string()],
            notification_tx: None,
        };

        assert_eq!(config.workspace_roots.len(), 3);
    }

    /// #249: `has_exited` must distinguish a live child from one that has
    /// already exited, since this is the signal the respawn path relies on
    /// to detect a crashed LSP server.
    ///
    /// Unix-only: spawns a real `sleep` subprocess, which is unavailable on
    /// the Windows CI runner.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_has_exited_reflects_child_process_state() {
        use lsp_types::ServerCapabilities;

        let mut mock_child = tokio::process::Command::new("sleep")
            .arg("2")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();

        let mock_stdin = mock_child.stdin.take().unwrap();
        let mock_stdout = mock_child.stdout.take().unwrap();

        let transport = LspTransport::new(mock_stdin, mock_stdout);
        let client = LspClient::from_transport(LspServerConfig::rust_analyzer(), transport);
        let (_, mock_notification_rx) = mpsc::channel(1);

        let mut server = LspServer {
            client,
            capabilities: ServerCapabilities::default(),
            server_name: None,
            position_encoding: PositionEncodingKind::UTF8,
            workspace_roots: Vec::new(),
            notification_rx: mock_notification_rx,
            process: LspProcess::unisolated(mock_child),
        };

        assert!(
            !server.has_exited().unwrap(),
            "freshly spawned `sleep 2` should still be running"
        );

        server.process.child.kill().await.unwrap();
        // `kill().await` waits for the process to actually exit, so the
        // very next `try_wait` reliably observes it as gone.
        assert!(
            server.has_exited().unwrap(),
            "killed child must report as exited"
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn shutdown_kills_language_server_descendants_before_returning() {
        use lsp_types::ServerCapabilities;

        let directory = tempfile::tempdir().unwrap();
        let descendant_pid_path = directory.path().join("descendant.pid");
        let mut command = tokio::process::Command::new("sh");
        command
            .args([
                "-c",
                "sleep 3600 & echo $! > \"$1\"; wait",
                "mcpls-lsp-fixture",
            ])
            .arg(&descendant_pid_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(true);
        command.as_std_mut().process_group(0);
        let child = command.spawn().unwrap();
        let parent_pid = child.id().unwrap();
        let client = LspClient::new(LspServerConfig::rust_analyzer());
        let (_, notification_rx) = mpsc::channel(1);
        let server = LspServer {
            client,
            capabilities: ServerCapabilities::default(),
            server_name: None,
            position_encoding: PositionEncodingKind::UTF8,
            workspace_roots: Vec::new(),
            notification_rx,
            process: LspProcess::isolated(child),
        };

        let descendant_pid = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if let Ok(pid) = std::fs::read_to_string(&descendant_pid_path)
                    && let Ok(pid) = pid.trim().parse::<u32>()
                {
                    break pid;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        assert!(server.shutdown().await.is_err());
        let parent_exists = Path::new(&format!("/proc/{parent_pid}")).exists();
        let descendant_exists = Path::new(&format!("/proc/{descendant_pid}")).exists();
        if descendant_exists {
            let _ = std::process::Command::new("kill")
                .args(["-KILL", &descendant_pid.to_string()])
                .status();
        }

        assert!(!parent_exists, "shutdown must reap the server parent");
        assert!(
            !descendant_exists,
            "shutdown must kill language-server descendants before returning"
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn failed_initialization_kills_language_server_descendants() {
        let directory = tempfile::tempdir().unwrap();
        let descendant_pid_path = directory.path().join("failed-init-descendant.pid");
        let mut config = LspServerConfig::rust_analyzer();
        config.command = "sh".to_string();
        config.args = vec![
            "-c".to_string(),
            "sleep 3600 </dev/null >/dev/null 2>&1 & echo $! > \"$1\"; exit 1".to_string(),
            "mcpls-lsp-fixture".to_string(),
            descendant_pid_path.display().to_string(),
        ];
        let result = LspServer::spawn(ServerInitConfig {
            server_config: config,
            workspace_roots: vec![directory.path().to_path_buf()],
            initialization_options: None,
            position_encodings: crate::config::default_position_encodings(),
            notification_tx: None,
        })
        .await;
        assert!(result.is_err());

        let descendant_pid = std::fs::read_to_string(&descendant_pid_path)
            .unwrap()
            .trim()
            .parse::<u32>()
            .unwrap();
        let descendant_exists = Path::new(&format!("/proc/{descendant_pid}")).exists();
        if descendant_exists {
            let _ = std::process::Command::new("kill")
                .args(["-KILL", &descendant_pid.to_string()])
                .status();
        }

        assert!(
            !descendant_exists,
            "failed initialization must not orphan language-server descendants"
        );
    }

    #[tokio::test]
    async fn test_lsp_server_getters() {
        use lsp_types::ServerCapabilities;

        let mock_child = tokio::process::Command::new("echo")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();

        let mock_stdin = tokio::process::Command::new("cat")
            .stdin(Stdio::piped())
            .spawn()
            .unwrap()
            .stdin
            .take()
            .unwrap();

        let mock_stdout = tokio::process::Command::new("echo")
            .stdout(Stdio::piped())
            .spawn()
            .unwrap()
            .stdout
            .take()
            .unwrap();

        let transport = LspTransport::new(mock_stdin, mock_stdout);
        let client = LspClient::from_transport(LspServerConfig::rust_analyzer(), transport);
        let (_, mock_notification_rx) = mpsc::channel(1);

        let server = LspServer {
            client,
            capabilities: ServerCapabilities::default(),
            server_name: Some("rust-analyzer".to_string()),
            position_encoding: PositionEncodingKind::UTF8,
            workspace_roots: vec![],
            notification_rx: mock_notification_rx,
            process: LspProcess::unisolated(mock_child),
        };

        assert_eq!(server.position_encoding(), PositionEncodingKind::UTF8);
        assert!(server.capabilities().text_document_sync.is_none());
        assert!(server.is_rust_analyzer());

        let debug_str = format!("{server:?}");
        assert!(debug_str.contains("LspServer"));
        assert!(debug_str.contains("<process tree>"));
    }

    #[test]
    fn test_server_init_result_new_empty() {
        let result = ServerInitResult::new();
        assert!(!result.has_servers());
        assert!(!result.all_failed());
        assert!(!result.partial_success());
        assert_eq!(result.server_count(), 0);
        assert_eq!(result.failure_count(), 0);
    }

    #[test]
    fn test_server_init_result_default() {
        let result = ServerInitResult::default();
        assert!(!result.has_servers());
        assert_eq!(result.server_count(), 0);
        assert_eq!(result.failure_count(), 0);
    }

    #[test]
    fn test_server_init_result_all_failures() {
        let mut result = ServerInitResult::new();

        result.add_failure(ServerSpawnFailure {
            server_id: ServerId::from("rust"),
            language_id: "rust".to_string(),
            command: "rust-analyzer".to_string(),
            message: "not found".to_string(),
        });

        result.add_failure(ServerSpawnFailure {
            server_id: ServerId::from("python"),
            language_id: "python".to_string(),
            command: "pyright".to_string(),
            message: "permission denied".to_string(),
        });

        assert!(!result.has_servers());
        assert!(result.all_failed());
        assert!(!result.partial_success());
        assert_eq!(result.server_count(), 0);
        assert_eq!(result.failure_count(), 2);
    }

    #[tokio::test]
    async fn test_server_init_result_all_success() {
        let mut result = ServerInitResult::new();

        let mock_child1 = tokio::process::Command::new("echo")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();

        let mock_stdin1 = tokio::process::Command::new("cat")
            .stdin(Stdio::piped())
            .spawn()
            .unwrap()
            .stdin
            .take()
            .unwrap();

        let mock_stdout1 = tokio::process::Command::new("echo")
            .stdout(Stdio::piped())
            .spawn()
            .unwrap()
            .stdout
            .take()
            .unwrap();

        let transport1 = LspTransport::new(mock_stdin1, mock_stdout1);
        let client1 = LspClient::from_transport(LspServerConfig::rust_analyzer(), transport1);
        let (_, mock_notification_rx1) = mpsc::channel(1);

        let server1 = LspServer {
            client: client1,
            capabilities: lsp_types::ServerCapabilities::default(),
            server_name: None,
            position_encoding: PositionEncodingKind::UTF8,
            workspace_roots: vec![],
            notification_rx: mock_notification_rx1,
            process: LspProcess::unisolated(mock_child1),
        };

        result.add_server("rust".to_string(), server1);

        assert!(result.has_servers());
        assert!(!result.all_failed());
        assert!(!result.partial_success());
        assert_eq!(result.server_count(), 1);
        assert_eq!(result.failure_count(), 0);
    }

    #[tokio::test]
    async fn test_server_init_result_partial_success() {
        let mut result = ServerInitResult::new();

        let mock_child = tokio::process::Command::new("echo")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();

        let mock_stdin = tokio::process::Command::new("cat")
            .stdin(Stdio::piped())
            .spawn()
            .unwrap()
            .stdin
            .take()
            .unwrap();

        let mock_stdout = tokio::process::Command::new("echo")
            .stdout(Stdio::piped())
            .spawn()
            .unwrap()
            .stdout
            .take()
            .unwrap();

        let transport = LspTransport::new(mock_stdin, mock_stdout);
        let client = LspClient::from_transport(LspServerConfig::rust_analyzer(), transport);
        let (_, mock_notification_rx) = mpsc::channel(1);

        let server = LspServer {
            client,
            capabilities: lsp_types::ServerCapabilities::default(),
            server_name: None,
            position_encoding: PositionEncodingKind::UTF8,
            workspace_roots: vec![],
            notification_rx: mock_notification_rx,
            process: LspProcess::unisolated(mock_child),
        };

        result.add_server("rust".to_string(), server);

        result.add_failure(ServerSpawnFailure {
            server_id: ServerId::from("python"),
            language_id: "python".to_string(),
            command: "pyright".to_string(),
            message: "not found".to_string(),
        });

        assert!(result.has_servers());
        assert!(!result.all_failed());
        assert!(result.partial_success());
        assert_eq!(result.server_count(), 1);
        assert_eq!(result.failure_count(), 1);
    }

    #[tokio::test]
    async fn test_server_init_result_multiple_servers() {
        let mut result = ServerInitResult::new();

        for i in 0..3 {
            let mock_child = tokio::process::Command::new("echo")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .unwrap();

            let mock_stdin = tokio::process::Command::new("cat")
                .stdin(Stdio::piped())
                .spawn()
                .unwrap()
                .stdin
                .take()
                .unwrap();

            let mock_stdout = tokio::process::Command::new("echo")
                .stdout(Stdio::piped())
                .spawn()
                .unwrap()
                .stdout
                .take()
                .unwrap();

            let transport = LspTransport::new(mock_stdin, mock_stdout);
            let config = if i == 0 {
                LspServerConfig::rust_analyzer()
            } else if i == 1 {
                LspServerConfig::pyright()
            } else {
                LspServerConfig::typescript()
            };
            let client = LspClient::from_transport(config.clone(), transport);
            let (_, mock_notification_rx) = mpsc::channel(1);

            let server = LspServer {
                client,
                capabilities: lsp_types::ServerCapabilities::default(),
                server_name: None,
                position_encoding: PositionEncodingKind::UTF8,
                workspace_roots: vec![],
                notification_rx: mock_notification_rx,
                process: LspProcess::unisolated(mock_child),
            };

            result.add_server(config.language_id, server);
        }

        assert!(result.has_servers());
        assert!(!result.all_failed());
        assert!(!result.partial_success());
        assert_eq!(result.server_count(), 3);
        assert_eq!(result.failure_count(), 0);
    }

    #[tokio::test]
    async fn test_server_init_result_replace_server() {
        let mut result = ServerInitResult::new();

        let mock_child1 = tokio::process::Command::new("echo")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();

        let mock_stdin1 = tokio::process::Command::new("cat")
            .stdin(Stdio::piped())
            .spawn()
            .unwrap()
            .stdin
            .take()
            .unwrap();

        let mock_stdout1 = tokio::process::Command::new("echo")
            .stdout(Stdio::piped())
            .spawn()
            .unwrap()
            .stdout
            .take()
            .unwrap();

        let transport1 = LspTransport::new(mock_stdin1, mock_stdout1);
        let client1 = LspClient::from_transport(LspServerConfig::rust_analyzer(), transport1);
        let (_, mock_notification_rx1) = mpsc::channel(1);

        let server1 = LspServer {
            client: client1,
            capabilities: lsp_types::ServerCapabilities::default(),
            server_name: None,
            position_encoding: PositionEncodingKind::UTF8,
            workspace_roots: vec![],
            notification_rx: mock_notification_rx1,
            process: LspProcess::unisolated(mock_child1),
        };

        result.add_server("rust".to_string(), server1);
        assert_eq!(result.server_count(), 1);

        let mock_child2 = tokio::process::Command::new("echo")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();

        let mock_stdin2 = tokio::process::Command::new("cat")
            .stdin(Stdio::piped())
            .spawn()
            .unwrap()
            .stdin
            .take()
            .unwrap();

        let mock_stdout2 = tokio::process::Command::new("echo")
            .stdout(Stdio::piped())
            .spawn()
            .unwrap()
            .stdout
            .take()
            .unwrap();

        let transport2 = LspTransport::new(mock_stdin2, mock_stdout2);
        let client2 = LspClient::from_transport(LspServerConfig::rust_analyzer(), transport2);
        let (_, mock_notification_rx2) = mpsc::channel(1);

        let server2 = LspServer {
            client: client2,
            capabilities: lsp_types::ServerCapabilities::default(),
            server_name: None,
            position_encoding: PositionEncodingKind::UTF16,
            workspace_roots: vec![],
            notification_rx: mock_notification_rx2,
            process: LspProcess::unisolated(mock_child2),
        };

        result.add_server("rust".to_string(), server2);
        assert_eq!(result.server_count(), 1);
    }

    #[test]
    fn test_server_init_result_debug() {
        let mut result = ServerInitResult::new();

        result.add_failure(ServerSpawnFailure {
            server_id: ServerId::from("rust"),
            language_id: "rust".to_string(),
            command: "rust-analyzer".to_string(),
            message: "not found".to_string(),
        });

        let debug_str = format!("{result:?}");
        assert!(debug_str.contains("ServerInitResult"));
    }

    #[test]
    fn test_server_init_result_multiple_failures() {
        let mut result = ServerInitResult::new();

        result.add_failure(ServerSpawnFailure {
            server_id: ServerId::from("python"),
            language_id: "python".to_string(),
            command: "pyright".to_string(),
            message: "not found".to_string(),
        });

        result.add_failure(ServerSpawnFailure {
            server_id: ServerId::from("typescript"),
            language_id: "typescript".to_string(),
            command: "tsserver".to_string(),
            message: "command not found".to_string(),
        });

        assert_eq!(result.failure_count(), 2);
        assert_eq!(result.server_count(), 0);
        assert!(result.all_failed());
        assert!(!result.partial_success());
    }

    #[tokio::test]
    async fn test_spawn_batch_empty_configs() {
        let configs: &[ServerInitConfig] = &[];
        let result = LspServer::spawn_batch(configs).await;

        assert!(!result.has_servers());
        assert!(!result.all_failed());
        assert!(!result.partial_success());
        assert_eq!(result.server_count(), 0);
        assert_eq!(result.failure_count(), 0);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn spawn_batch_initializes_servers_concurrently() {
        let directory = tempfile::tempdir().unwrap();
        let command = write_coordinated_lsp(&directory);
        let signal = directory.path().join("initialization-signal");
        let server = |language_id: &str, env| ServerInitConfig {
            server_config: LspServerConfig {
                language_id: language_id.to_string(),
                command: command.display().to_string(),
                args: vec![],
                env,
                file_patterns: vec![],
                initialization_options: None,
                timeout_seconds: 1,
                request_timeout_seconds: 30,
                heuristics: None,
                name: None,
                handles: None,
            },
            workspace_roots: vec![directory.path().to_path_buf()],
            initialization_options: None,
            position_encodings: crate::config::default_position_encodings(),
            notification_tx: None,
        };
        let configs = [
            server(
                "waiter",
                HashMap::from([("MCPLS_WAIT_FILE".to_string(), signal.display().to_string())]),
            ),
            server(
                "signaler",
                HashMap::from([(
                    "MCPLS_SIGNAL_FILE".to_string(),
                    signal.display().to_string(),
                )]),
            ),
        ];

        let result = LspServer::spawn_batch(&configs).await;

        assert_eq!(result.failure_count(), 0);
        assert_eq!(result.server_count(), 2);
        for server in result.servers.into_values() {
            let process_id = server.process.id();
            server.shutdown().await.unwrap();
            #[cfg(target_os = "linux")]
            assert!(
                process_id.is_none_or(|process_id| {
                    !std::path::Path::new(&format!("/proc/{process_id}")).exists()
                }),
                "shutdown must reap the language-server process"
            );
        }
    }

    #[tokio::test]
    async fn test_spawn_batch_single_invalid_config() {
        let configs = vec![ServerInitConfig {
            server_config: LspServerConfig {
                language_id: "rust".to_string(),
                command: "nonexistent-command-12345".to_string(),
                args: vec![],
                env: std::collections::HashMap::new(),
                file_patterns: vec!["**/*.rs".to_string()],
                initialization_options: None,
                timeout_seconds: 10,
                request_timeout_seconds: 10,
                heuristics: None,
                name: None,
                handles: None,
            },
            workspace_roots: vec![],
            initialization_options: None,
            position_encodings: vec!["utf-8".to_string(), "utf-16".to_string()],
            notification_tx: None,
        }];

        let result = LspServer::spawn_batch(&configs).await;

        assert!(!result.has_servers());
        assert!(result.all_failed());
        assert!(!result.partial_success());
        assert_eq!(result.server_count(), 0);
        assert_eq!(result.failure_count(), 1);

        let failure = &result.failures[0];
        assert_eq!(failure.language_id, "rust");
        assert_eq!(failure.command, "nonexistent-command-12345");
        assert!(failure.message.contains("spawn"));
    }

    #[tokio::test]
    async fn test_spawn_batch_all_invalid_configs() {
        let configs = vec![
            ServerInitConfig {
                server_config: LspServerConfig {
                    language_id: "rust".to_string(),
                    command: "nonexistent-rust-analyzer".to_string(),
                    args: vec![],
                    env: std::collections::HashMap::new(),
                    file_patterns: vec!["**/*.rs".to_string()],
                    initialization_options: None,
                    timeout_seconds: 10,
                    request_timeout_seconds: 10,
                    heuristics: None,
                    name: None,
                    handles: None,
                },
                workspace_roots: vec![],
                initialization_options: None,
                position_encodings: vec!["utf-8".to_string(), "utf-16".to_string()],
                notification_tx: None,
            },
            ServerInitConfig {
                server_config: LspServerConfig {
                    language_id: "python".to_string(),
                    command: "nonexistent-pyright".to_string(),
                    args: vec![],
                    env: std::collections::HashMap::new(),
                    file_patterns: vec!["**/*.py".to_string()],
                    initialization_options: None,
                    timeout_seconds: 10,
                    request_timeout_seconds: 10,
                    heuristics: None,
                    name: None,
                    handles: None,
                },
                workspace_roots: vec![],
                initialization_options: None,
                position_encodings: vec!["utf-8".to_string(), "utf-16".to_string()],
                notification_tx: None,
            },
            ServerInitConfig {
                server_config: LspServerConfig {
                    language_id: "typescript".to_string(),
                    command: "nonexistent-tsserver".to_string(),
                    args: vec![],
                    env: std::collections::HashMap::new(),
                    file_patterns: vec!["**/*.ts".to_string()],
                    initialization_options: None,
                    timeout_seconds: 10,
                    request_timeout_seconds: 10,
                    heuristics: None,
                    name: None,
                    handles: None,
                },
                workspace_roots: vec![],
                initialization_options: None,
                position_encodings: vec!["utf-8".to_string(), "utf-16".to_string()],
                notification_tx: None,
            },
        ];

        let result = LspServer::spawn_batch(&configs).await;

        assert!(!result.has_servers());
        assert!(result.all_failed());
        assert!(!result.partial_success());
        assert_eq!(result.server_count(), 0);
        assert_eq!(result.failure_count(), 3);

        let failure_languages: Vec<_> = result
            .failures
            .iter()
            .map(|f| f.language_id.as_str())
            .collect();
        assert!(failure_languages.contains(&"rust"));
        assert!(failure_languages.contains(&"python"));
        assert!(failure_languages.contains(&"typescript"));
    }

    #[tokio::test]
    async fn test_spawn_batch_multiple_invalid_configs_ordering() {
        let configs = vec![
            ServerInitConfig {
                server_config: LspServerConfig {
                    language_id: "lang1".to_string(),
                    command: "cmd1-nonexistent".to_string(),
                    args: vec![],
                    env: std::collections::HashMap::new(),
                    file_patterns: vec![],
                    initialization_options: None,
                    timeout_seconds: 10,
                    request_timeout_seconds: 10,
                    heuristics: None,
                    name: None,
                    handles: None,
                },
                workspace_roots: vec![],
                initialization_options: None,
                position_encodings: vec!["utf-8".to_string(), "utf-16".to_string()],
                notification_tx: None,
            },
            ServerInitConfig {
                server_config: LspServerConfig {
                    language_id: "lang2".to_string(),
                    command: "cmd2-nonexistent".to_string(),
                    args: vec![],
                    env: std::collections::HashMap::new(),
                    file_patterns: vec![],
                    initialization_options: None,
                    timeout_seconds: 10,
                    request_timeout_seconds: 10,
                    heuristics: None,
                    name: None,
                    handles: None,
                },
                workspace_roots: vec![],
                initialization_options: None,
                position_encodings: vec!["utf-8".to_string(), "utf-16".to_string()],
                notification_tx: None,
            },
        ];

        let result = LspServer::spawn_batch(&configs).await;

        assert_eq!(result.failure_count(), 2);

        assert_eq!(result.failures[0].language_id, "lang1");
        assert_eq!(result.failures[0].command, "cmd1-nonexistent");

        assert_eq!(result.failures[1].language_id, "lang2");
        assert_eq!(result.failures[1].command, "cmd2-nonexistent");
    }

    /// Wire-level regression test for #287: proves the *configured*
    /// `position_encodings` (not the old hardcoded `[UTF8, UTF16]`) actually
    /// reaches `capabilities.general.positionEncodings` in the `initialize`
    /// request body, by capturing the real bytes `LspServer::initialize`
    /// writes over a piped `cat` subprocess standing in for the LSP server.
    /// Mirrors the `fake_lsp_client`/`FakeServer` pattern in
    /// `client.rs::tests::retry_behavior`.
    mod initialize_wire {
        use std::process::Stdio;

        use serde_json::Value;
        use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
        use tokio::process::{Child, ChildStdin, ChildStdout, Command};

        use super::*;
        use crate::lsp::client::LspClient;

        struct FakeServer {
            _write_half: Child,
            _read_half: Child,
            read_half_stdin: ChildStdin,
            write_stdout: ChildStdout,
        }

        fn fake_lsp_client() -> (LspClient, FakeServer) {
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

            let transport = LspTransport::new(write_stdin, read_stdout);
            let client = LspClient::from_transport(LspServerConfig::rust_analyzer(), transport);

            (
                client,
                FakeServer {
                    _write_half: write_half,
                    _read_half: read_half,
                    read_half_stdin: read_stdin,
                    write_stdout,
                },
            )
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

        #[tokio::test]
        async fn test_initialize_sends_configured_position_encodings() {
            let (client, mut server) = fake_lsp_client();

            let config = ServerInitConfig {
                server_config: LspServerConfig::rust_analyzer(),
                workspace_roots: vec![],
                initialization_options: None,
                position_encodings: vec!["utf-32".to_string(), "utf-8".to_string()],
                notification_tx: None,
            };

            let init_task =
                tokio::spawn(async move { LspServer::initialize(&client, &config).await });

            let mut reader = BufReader::new(&mut server.write_stdout);
            let request = read_framed_message(&mut reader).await;

            assert_eq!(request["method"], "initialize");
            assert_eq!(
                request["params"]["capabilities"]["general"]["positionEncodings"],
                serde_json::json!(["utf-32", "utf-8"]),
                "initialize request must carry the configured encoding order, not the \
                 hardcoded [UTF8, UTF16] default"
            );

            write_success_response(
                &mut server.read_half_stdin,
                &request["id"].clone(),
                serde_json::json!({ "capabilities": {} }),
            )
            .await;

            // The response written above must let `initialize` complete successfully.
            init_task.await.unwrap().unwrap();
        }
    }

    #[tokio::test]
    async fn test_spawn_batch_logs_each_failure() {
        let configs = vec![
            ServerInitConfig {
                server_config: LspServerConfig {
                    language_id: "test1".to_string(),
                    command: "nonexistent-test1".to_string(),
                    args: vec![],
                    env: std::collections::HashMap::new(),
                    file_patterns: vec![],
                    initialization_options: None,
                    timeout_seconds: 10,
                    request_timeout_seconds: 10,
                    heuristics: None,
                    name: None,
                    handles: None,
                },
                workspace_roots: vec![],
                initialization_options: None,
                position_encodings: vec!["utf-8".to_string(), "utf-16".to_string()],
                notification_tx: None,
            },
            ServerInitConfig {
                server_config: LspServerConfig {
                    language_id: "test2".to_string(),
                    command: "nonexistent-test2".to_string(),
                    args: vec![],
                    env: std::collections::HashMap::new(),
                    file_patterns: vec![],
                    initialization_options: None,
                    timeout_seconds: 10,
                    request_timeout_seconds: 10,
                    heuristics: None,
                    name: None,
                    handles: None,
                },
                workspace_roots: vec![],
                initialization_options: None,
                position_encodings: vec!["utf-8".to_string(), "utf-16".to_string()],
                notification_tx: None,
            },
        ];

        let result = LspServer::spawn_batch(&configs).await;

        assert_eq!(result.failure_count(), 2);
        assert_eq!(result.failures[0].language_id, "test1");
        assert_eq!(result.failures[1].language_id, "test2");
    }

    /// Minimal [`LspServerConfig`] for `build_command` tests, where only
    /// `command`/`args`/`env` matter.
    fn bare_server_config(env: HashMap<String, String>) -> LspServerConfig {
        LspServerConfig {
            language_id: "test".to_string(),
            command: "irrelevant-for-build-command".to_string(),
            args: vec![],
            env,
            file_patterns: vec![],
            initialization_options: None,
            timeout_seconds: 5,
            request_timeout_seconds: 5,
            heuristics: None,
            name: None,
            handles: None,
        }
    }

    /// Collects the env vars a `Command` would set, resolving `env_clear`
    /// removals (`None` values from `get_envs`) away so the map reflects
    /// what the child process would actually see.
    fn effective_envs(command: &Command) -> HashMap<String, String> {
        command
            .as_std()
            .get_envs()
            .filter_map(|(k, v)| {
                v.map(|v| {
                    (
                        k.to_string_lossy().into_owned(),
                        v.to_string_lossy().into_owned(),
                    )
                })
            })
            .collect()
    }

    /// Regression test for #236/#246: a spawned LSP server used to inherit
    /// mcpls's entire environment. `build_command` must only pass through
    /// `ENV_PASSTHROUGH` keys from `parent_env`, not arbitrary ones.
    #[test]
    fn test_build_command_excludes_non_allowlisted_parent_env_vars() {
        let config = bare_server_config(HashMap::new());
        let command = LspServer::build_command(&config, |key| match key {
            "PATH" => Some("/parent/bin".into()),
            "MCPLS_TEST_LEAK_CANARY" => Some("should-not-reach-child".into()),
            _ => None,
        });

        let envs = effective_envs(&command);

        assert!(
            !envs.contains_key("MCPLS_TEST_LEAK_CANARY"),
            "non-allowlisted parent env var leaked into child command: {envs:?}"
        );

        // The assertion above is provably vacuous on its own:
        // `Command::get_envs()` only reports explicit `.env()`/`.envs()`
        // modifications and is blind to whether `.env_clear()` was called,
        // and `build_command`'s passthrough loop never even queries
        // `parent_env` for a key outside `ENV_PASSTHROUGH`, so it would
        // pass unchanged even if `.env_clear()` were deleted from
        // `build_command` entirely. `std::process::Command`'s `Debug` impl
        // does encode clearing, prefixing the formatted command with
        // `env -i ` on Unix once `.env_clear()` has run; assert on that to
        // actually guard against the clear being removed.
        #[cfg(unix)]
        assert!(
            format!("{:?}", command.as_std()).starts_with("env -i "),
            "build_command must call .env_clear() so the child doesn't inherit the full parent environment"
        );
    }

    /// Regression test for #236/#246: allowlisted vars present in the parent
    /// (e.g. `PATH`) must still reach the child.
    #[test]
    fn test_build_command_passes_through_allowlisted_env_vars() {
        let config = bare_server_config(HashMap::new());
        let command =
            LspServer::build_command(&config, |key| (key == "PATH").then(|| "/parent/bin".into()));

        let envs = effective_envs(&command);

        assert_eq!(envs.get("PATH"), Some(&"/parent/bin".to_string()));
    }

    #[test]
    fn test_project_environment_overrides_parent_without_leaking_secrets() {
        let mut config = bare_server_config(HashMap::new());
        let project_environment = HashMap::from([
            ("PATH".to_string(), Some("/project/bin".to_string())),
            (
                "MCPLS_TEST_LEAK_CANARY".to_string(),
                Some("should-not-reach-child".to_string()),
            ),
        ]);

        apply_project_environment(&mut config, &project_environment);
        let command =
            LspServer::build_command(&config, |key| (key == "PATH").then(|| "/parent/bin".into()));
        let envs = effective_envs(&command);

        assert_eq!(envs.get("PATH"), Some(&"/project/bin".to_string()));
        assert!(!envs.contains_key("MCPLS_TEST_LEAK_CANARY"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn project_environment_command_times_out_without_waiting_for_child() {
        let root = tempfile::tempdir().unwrap();
        let started = std::time::Instant::now();

        let environment = command_environment_with_timeout(
            "sh",
            ["-c", "sleep 1"],
            root.path(),
            std::time::Duration::from_millis(20),
        )
        .await;

        assert!(environment.is_none());
        assert!(started.elapsed() < std::time::Duration::from_millis(500));
    }

    /// Regression test for #247: `LspServerConfig::env` entries must reach
    /// the spawned child (previously dead configuration).
    #[test]
    fn test_build_command_includes_configured_env_vars() {
        let mut env = HashMap::new();
        env.insert(
            "MCPLS_TEST_CONFIGURED".to_string(),
            "from-server-config".to_string(),
        );
        let config = bare_server_config(env);
        let command = LspServer::build_command(&config, |_| None);

        let envs = effective_envs(&command);

        assert_eq!(
            envs.get("MCPLS_TEST_CONFIGURED"),
            Some(&"from-server-config".to_string())
        );
    }

    /// Regression test for #247: a `LspServerConfig::env` entry must be able
    /// to override an allowlisted passthrough value, since `config.env` is
    /// applied after the passthrough loop in `build_command`.
    #[test]
    fn test_build_command_configured_env_overrides_allowlisted_var() {
        let mut env = HashMap::new();
        env.insert("PATH".to_string(), "/configured/override/path".to_string());
        let config = bare_server_config(env);
        let command =
            LspServer::build_command(&config, |key| (key == "PATH").then(|| "/parent/bin".into()));

        let envs = effective_envs(&command);

        assert_eq!(
            envs.get("PATH"),
            Some(&"/configured/override/path".to_string())
        );
    }

    /// #174 §8/S2 regression: `register_servers`'s diagnostics-cache flags
    /// must be computed from the *rebound* router, not the pre-rebind view.
    /// Sets up a `python` config where a narrow "diagnostics-only" server
    /// (`pyright-diag`) is configured but never actually registers (as if
    /// it failed to spawn), leaving only a catch-all (`pylsp`) live. Before
    /// the fix, computing the flags from the pre-rebind router would resolve
    /// `Diagnostics` to the dead `pyright-diag` for every survivor, so
    /// `pylsp` would be flagged `false` and the diagnostics cache would go
    /// silently dark for `python` despite a live server being available.
    #[tokio::test]
    async fn test_register_servers_computes_diagnostics_flags_from_rebound_router() {
        use crate::bridge::Translator;
        use crate::config::{ServerId, ToolKind, ToolRouter};

        let pylsp_id = ServerId::from("pylsp");
        let configs = vec![
            LspServerConfig {
                language_id: "python".to_string(),
                command: "pyright-langserver".to_string(),
                args: vec![],
                env: std::collections::HashMap::new(),
                file_patterns: vec![],
                initialization_options: None,
                timeout_seconds: 30,
                request_timeout_seconds: 30,
                heuristics: None,
                name: Some("pyright-diag".to_string()),
                handles: Some(vec![ToolKind::Diagnostics]),
            },
            LspServerConfig {
                language_id: "python".to_string(),
                command: "pylsp".to_string(),
                args: vec![],
                env: std::collections::HashMap::new(),
                file_patterns: vec![],
                initialization_options: None,
                timeout_seconds: 30,
                request_timeout_seconds: 30,
                heuristics: None,
                name: Some("pylsp".to_string()),
                handles: None,
            },
        ];
        let router = ToolRouter::from_configs(&configs).unwrap();
        let translator = Translator::new().with_router(router);

        // Only pylsp actually registers; pyright-diag never spawned.
        let mut result = ServerInitResult::new();
        result.add_server(pylsp_id.clone(), fake_lsp_server());

        let registered_ids = result
            .servers
            .keys()
            .cloned()
            .collect::<std::collections::HashSet<_>>();
        translator.rebind_router(&registered_ids);

        assert!(
            translator.is_diagnostics_route("python", &pylsp_id),
            "pylsp must inherit the diagnostics route once pyright-diag is \
             known dead, and the flag must reflect that post-rebind state"
        );
    }
}
