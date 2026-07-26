//! MCP to LSP translation layer.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use lsp_types::{
    CallHierarchyIncomingCall, CallHierarchyIncomingCallsParams, CallHierarchyItem,
    CallHierarchyOutgoingCall, CallHierarchyOutgoingCallsParams,
    CallHierarchyPrepareParams as LspCallHierarchyPrepareParams, CompletionParams,
    CompletionTriggerKind, DidChangeTextDocumentParams, DocumentFormattingParams, DocumentSymbol,
    DocumentSymbolParams, FormattingOptions, GotoDefinitionParams, Hover, HoverContents,
    HoverParams as LspHoverParams, InlayHintLabel, InlayHintParams, MarkedString,
    PartialResultParams, ReferenceContext, ReferenceParams, RenameParams as LspRenameParams,
    SignatureHelpParams as LspSignatureHelpParams, TextDocumentContentChangeEvent,
    TextDocumentIdentifier, TextDocumentPositionParams, VersionedTextDocumentIdentifier,
    WorkDoneProgressParams, WorkspaceEdit, WorkspaceSymbolParams as LspWorkspaceSymbolParams,
};
use serde::{Deserialize, Serialize};
use tokio::{sync::mpsc, time::Duration};

use super::notifications::RedactionPolicy;
use super::state::{ResourceLimits, detect_language, path_to_uri};
use super::{DocumentTracker, NotificationCache};
use crate::bridge::encoding::mcp_to_lsp_position;
use crate::config::{LspServerConfig, ProjectConfig};
use crate::error::{Error, Result};
use crate::lsp::{LspClient, LspNotification, LspServer, ServerInitConfig};

/// Health of a project activation across its configured language servers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivationHealth {
    /// Every applicable language server initialized successfully.
    Ready,
    /// At least one applicable language server failed while another started.
    Degraded,
}

/// Language-server handles produced by one project activation.
#[derive(Debug)]
pub struct ProjectActivation {
    notification_receivers: Vec<mpsc::Receiver<LspNotification>>,
    health: ActivationHealth,
}

impl ProjectActivation {
    pub(crate) const fn ready() -> Self {
        Self::new(Vec::new(), ActivationHealth::Ready)
    }

    pub(crate) const fn new(
        notification_receivers: Vec<mpsc::Receiver<LspNotification>>,
        health: ActivationHealth,
    ) -> Self {
        Self {
            notification_receivers,
            health,
        }
    }

    /// Return the activation health.
    #[must_use]
    pub const fn health(&self) -> ActivationHealth {
        self.health
    }

    /// Consume the activation and return notification streams for its servers.
    #[must_use]
    pub fn into_notification_receivers(self) -> Vec<mpsc::Receiver<LspNotification>> {
        self.notification_receivers
    }
}

/// Translator handles MCP tool calls by converting them to LSP requests.
#[derive(Debug)]
pub struct Translator {
    /// LSP clients indexed by language ID.
    lsp_clients: HashMap<String, LspClient>,
    /// LSP servers indexed by language ID (held for lifetime management).
    lsp_servers: HashMap<String, LspServer>,
    /// Document state tracker.
    document_tracker: DocumentTracker,
    /// Notification cache for LSP server notifications.
    notification_cache: NotificationCache,
    /// Allowed workspace roots for path validation.
    workspace_roots: Vec<PathBuf>,
    /// Custom file extension to language ID mappings.
    extension_map: HashMap<String, String>,
    /// Languages that are configured + applicable but whose LSP server may not
    /// have finished initializing yet (background init). Used to return a clear
    /// "still initializing" error instead of "no server configured".
    expected_languages: HashSet<String>,
    /// Configured LSP servers indexed by language ID for on-demand workspace changes.
    lsp_configs: HashMap<String, LspServerConfig>,
    /// Workspace roots used by the registered LSP server for each language ID.
    lsp_roots: HashMap<String, Vec<PathBuf>>,
    /// Configured environment values that must not escape through notifications.
    redaction_policy: RedactionPolicy,
    /// Maximum ancestor/recursive marker search depth.
    heuristics_max_depth: Option<usize>,
}

const fn shutdown_error_is_recoverable(error: &Error) -> bool {
    matches!(error, Error::ServerTerminated)
}

async fn shutdown_servers(servers: HashMap<String, LspServer>) -> Result<()> {
    let mut first_error = None;
    for server in servers.into_values() {
        if let Err(error) = server.shutdown().await
            && !shutdown_error_is_recoverable(&error)
            && first_error.is_none()
        {
            first_error = Some(error);
        }
    }
    first_error.map_or(Ok(()), Err)
}

/// Configuration snapshot used to construct an isolated project translator.
#[derive(Debug, Clone, Serialize, Default)]
pub struct TranslatorTemplate {
    extension_map: HashMap<String, String>,
    lsp_configs: Vec<LspServerConfig>,
    redaction_patterns: Vec<String>,
    heuristics_max_depth: Option<usize>,
    edit_safety: Option<crate::config::EditSafetyConfig>,
}

impl TranslatorTemplate {
    /// Build the daemon template directly from its declarative configuration.
    #[must_use]
    pub(crate) fn from_server_config(config: &crate::config::ServerConfig) -> Self {
        let mut template = Self::from_configuration(
            config.build_effective_extension_map(),
            config.lsp_servers.clone(),
            Some(config.workspace.heuristics_max_depth),
        );
        template.edit_safety.clone_from(&config.daemon.edit_safety);
        template
    }

    /// Build an immutable project configuration without creating a live
    /// translator or language-server runtime.
    #[must_use]
    pub(crate) const fn from_configuration(
        extension_map: HashMap<String, String>,
        lsp_configs: Vec<LspServerConfig>,
        heuristics_max_depth: Option<usize>,
    ) -> Self {
        Self {
            extension_map,
            lsp_configs,
            redaction_patterns: Vec::new(),
            heuristics_max_depth,
            edit_safety: None,
        }
    }

    pub(crate) fn rust_server_config(&self) -> Option<&LspServerConfig> {
        self.lsp_configs
            .iter()
            .find(|config| config.language_id.eq_ignore_ascii_case("rust"))
    }

    pub(crate) const fn heuristics_max_depth(&self) -> Option<usize> {
        self.heuristics_max_depth
    }

    /// Apply optional runtime project overrides to a daemon configuration.
    #[must_use]
    pub fn with_project_config(mut self, config: &ProjectConfig) -> Self {
        if let Some(lsp_servers) = &config.lsp_servers {
            self.lsp_configs.clone_from(lsp_servers);
        }
        if let Some(max_depth) = config.heuristics_max_depth {
            self.heuristics_max_depth = Some(max_depth);
        }
        if let Some(patterns) = &config.redaction_patterns {
            self.redaction_patterns.clone_from(patterns);
        }
        if let Some(edit_safety) = &config.edit_safety {
            self.edit_safety = Some(edit_safety.clone());
        }
        self
    }

    pub(crate) const fn edit_safety(&self) -> Option<&crate::config::EditSafetyConfig> {
        self.edit_safety.as_ref()
    }

    pub(crate) fn same_configuration(&self, other: &Self) -> bool {
        serde_json::to_vec(self).ok() == serde_json::to_vec(other).ok()
    }
}

impl Translator {
    /// Create a new translator.
    #[must_use]
    pub fn new() -> Self {
        Self {
            lsp_clients: HashMap::new(),
            lsp_servers: HashMap::new(),
            document_tracker: DocumentTracker::new(ResourceLimits::default(), HashMap::new()),
            notification_cache: NotificationCache::new(),
            workspace_roots: vec![],
            extension_map: HashMap::new(),
            expected_languages: HashSet::new(),
            lsp_configs: HashMap::new(),
            lsp_roots: HashMap::new(),
            redaction_policy: RedactionPolicy::default(),
            heuristics_max_depth: None,
        }
    }

    /// Capture configuration without carrying live clients, servers, or documents.
    #[must_use]
    pub fn configuration_template(&self) -> TranslatorTemplate {
        TranslatorTemplate::from_configuration(
            self.extension_map.clone(),
            self.lsp_configs.values().cloned().collect(),
            self.heuristics_max_depth,
        )
    }

    /// Set the workspace roots for path validation.
    pub fn set_workspace_roots(&mut self, roots: Vec<PathBuf>) {
        self.workspace_roots = roots;
    }

    /// Return the workspace roots owned by this translator.
    #[must_use]
    pub fn workspace_roots(&self) -> &[PathBuf] {
        &self.workspace_roots
    }

    /// Return configured language IDs in deterministic order.
    #[must_use]
    pub fn configured_language_ids(&self) -> Vec<String> {
        let mut languages: Vec<_> = self.lsp_configs.keys().cloned().collect();
        languages.sort();
        languages
    }

    /// Return active language IDs in deterministic order.
    #[must_use]
    pub fn active_language_ids(&self) -> Vec<String> {
        let mut languages: Vec<_> = self.lsp_clients.keys().cloned().collect();
        languages.sort();
        languages
    }

    /// Return whether the active servers already own exactly these roots.
    #[must_use]
    pub fn has_active_workspace_roots(&self, roots: &[PathBuf]) -> bool {
        !self.lsp_clients.is_empty()
            && self.expected_languages.is_empty()
            && Self::same_workspace_roots(&self.workspace_roots, roots)
            && self.lsp_clients.keys().all(|language_id| {
                self.lsp_configs
                    .get(language_id)
                    .map(|config| self.server_workspace_roots(config, roots))
                    .zip(self.lsp_roots.get(language_id))
                    .is_some_and(|(expected, registered)| {
                        Self::same_workspace_roots(registered, &expected)
                    })
            })
    }

    /// Return negotiated capabilities for active language servers.
    ///
    /// An optional language ID narrows the result without exposing command or
    /// environment configuration.
    ///
    /// # Errors
    ///
    /// Returns a JSON serialization error if an LSP capability payload cannot
    /// be represented safely.
    pub fn server_capabilities(&self, language_id: Option<&str>) -> Result<Vec<ServerCapability>> {
        let mut capabilities = self
            .lsp_servers
            .iter()
            .filter(|(id, _)| {
                language_id.is_none_or(|requested| id.eq_ignore_ascii_case(requested))
            })
            .map(|(language_id, server)| {
                Ok(ServerCapability {
                    language_id: language_id.clone(),
                    position_encoding: format!("{:?}", server.position_encoding()),
                    capabilities: serde_json::to_value(server.capabilities())?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        capabilities.sort_by(|left, right| left.language_id.cmp(&right.language_id));
        Ok(capabilities)
    }

    /// Return the number of open documents tracked by this translator.
    #[must_use]
    pub fn open_document_count(&self) -> usize {
        self.document_tracker.open_paths().count()
    }

    /// Mark the set of languages whose LSP servers are expected (configured +
    /// applicable) but may still be initializing in the background.
    pub fn set_expected_languages(&mut self, languages: HashSet<String>) {
        self.expected_languages = languages;
    }

    /// Configure LSP server definitions used for lazy project-root switches.
    pub fn set_lsp_configs(&mut self, configs: Vec<LspServerConfig>, max_depth: Option<usize>) {
        self.redaction_policy = RedactionPolicy::from_secrets(
            configs
                .iter()
                .flat_map(|config| config.env.values())
                .cloned(),
        );
        self.lsp_configs = configs
            .into_iter()
            .map(|config| (config.language_id.clone(), config))
            .collect();
        self.heuristics_max_depth = max_depth;
    }

    /// Configure literal values that must not escape through notifications.
    pub fn set_redaction_patterns(&mut self, patterns: Vec<String>) {
        let secrets = self
            .lsp_configs
            .values()
            .flat_map(|config| config.env.values())
            .cloned()
            .chain(patterns);
        self.redaction_policy = RedactionPolicy::from_secrets(secrets);
    }

    /// Clear the expected-languages set (e.g. after background init failed).
    pub fn clear_expected_languages(&mut self) {
        self.expected_languages.clear();
    }

    /// Return whether any configured language server is still loading its
    /// workspace after the LSP handshake completed.
    #[must_use]
    pub fn is_initializing(&self) -> bool {
        !self.expected_languages.is_empty()
    }

    /// Configure custom file extension mappings.
    ///
    /// This method sets the extension map and updates the document tracker
    /// to use the same mappings for language detection.
    #[must_use]
    pub fn with_extensions(mut self, extension_map: HashMap<String, String>) -> Self {
        self.document_tracker =
            DocumentTracker::new(ResourceLimits::default(), extension_map.clone());
        self.extension_map = extension_map;
        self
    }

    /// Register an LSP client for a language.
    pub fn register_client(&mut self, language_id: String, client: LspClient) {
        self.lsp_clients.insert(language_id, client);
    }

    /// Register an LSP server for a language.
    pub fn register_server(&mut self, language_id: String, server: LspServer) {
        self.lsp_servers.insert(language_id, server);
    }

    /// Remember the workspace root for a registered language server.
    pub fn register_server_root(&mut self, language_id: String, root: PathBuf) {
        self.register_server_roots(language_id, vec![root]);
    }

    /// Remember the workspace roots for a registered language server.
    pub fn register_server_roots(&mut self, language_id: String, roots: Vec<PathBuf>) {
        self.lsp_roots.insert(language_id, roots);
    }

    /// Start one project and track language-server readiness asynchronously.
    ///
    /// This is the explicit project boundary used by the `project_activate`
    /// MCP tool. Servers are exposed to code-intelligence requests after their
    /// initial LSP handshake; rust-analyzer continues loading its workspace in
    /// the background.
    ///
    /// # Errors
    ///
    /// Returns an error when no configured server applies or a server cannot be
    /// started. Background workspace loading does not block activation.
    pub async fn activate_project(&mut self, root: PathBuf) -> Result<ProjectActivation> {
        self.activate_project_with_roots(vec![root]).await
    }

    /// Activate configured language servers for a linked set of workspace roots.
    ///
    /// # Errors
    ///
    /// Returns an error when no configured server applies or a server cannot be
    /// started. Background workspace loading does not block activation.
    pub async fn activate_project_with_roots(
        &mut self,
        roots: Vec<PathBuf>,
    ) -> Result<ProjectActivation> {
        if roots.is_empty() {
            return Err(Error::NoServerConfigured);
        }
        let configs: Vec<_> = self
            .lsp_configs
            .values()
            .filter(|config| {
                roots
                    .iter()
                    .any(|root| config.should_spawn(root, self.heuristics_max_depth))
            })
            .cloned()
            .collect();

        if configs.is_empty() {
            return Err(Error::NoServerConfigured);
        }

        let mut pending = Vec::new();
        for config in &configs {
            let language_id = &config.language_id;
            if self.can_reuse_server(config, &roots) {
                continue;
            }

            if let Some(server) = self.lsp_servers.remove(language_id) {
                let _ = server.shutdown().await;
            }
            self.lsp_clients.remove(language_id);
            self.lsp_roots.remove(language_id);
            pending.push(config.clone());
        }

        if pending.is_empty() {
            self.set_workspace_roots(roots);
            return Ok(ProjectActivation::ready());
        }

        let expected_languages = pending
            .iter()
            .filter(|config| config.language_id.eq_ignore_ascii_case("rust"))
            .map(|config| config.language_id.clone())
            .collect();
        self.set_expected_languages(expected_languages);

        let server_configs = pending
            .iter()
            .map(|config| {
                let server_roots = self.server_workspace_roots(config, &roots);
                Ok(ServerInitConfig {
                    server_config: config.clone(),
                    workspace_roots: server_roots.clone(),
                    initialization_options: linked_project_initialization_options(
                        config,
                        &server_roots,
                    )?,
                    notification_tx: None,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let result = LspServer::spawn_batch(&server_configs).await;

        if result.all_failed() {
            self.clear_expected_languages();
            let message = result
                .failures
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ");
            return Err(Error::LspInitFailed { message });
        }

        let failures = result.failures;
        let servers = result.servers;
        let health = if failures.is_empty() {
            ActivationHealth::Ready
        } else {
            ActivationHealth::Degraded
        };
        self.set_workspace_roots(roots);
        let mut notification_receivers = Vec::new();
        for (language_id, mut server) in servers {
            let client = server.client().clone();
            let roots = server.workspace_roots().to_vec();
            notification_receivers.push(server.take_notification_rx());
            self.register_server_roots(language_id.clone(), roots);
            self.register_client(language_id.clone(), client);
            self.register_server(language_id, server);
        }
        self.reopen_tracked_documents().await?;
        self.clear_expected_languages();
        Ok(ProjectActivation::new(notification_receivers, health))
    }

    fn same_workspace_roots(existing: &[PathBuf], requested: &[PathBuf]) -> bool {
        existing.len() == requested.len()
            && requested
                .iter()
                .all(|root| existing.iter().any(|existing| existing == root))
    }

    fn can_reuse_server(&self, config: &LspServerConfig, requested_roots: &[PathBuf]) -> bool {
        self.lsp_clients.contains_key(&config.language_id)
            && self
                .lsp_roots
                .get(&config.language_id)
                .is_some_and(|existing| {
                    Self::same_workspace_roots(
                        existing,
                        &self.server_workspace_roots(config, requested_roots),
                    )
                })
    }

    fn server_workspace_roots(&self, config: &LspServerConfig, roots: &[PathBuf]) -> Vec<PathBuf> {
        let mut server_roots = roots
            .iter()
            .flat_map(|root| {
                config.heuristics.as_ref().map_or_else(
                    || vec![root.clone()],
                    |heuristics| heuristics.matching_roots(root, self.heuristics_max_depth),
                )
            })
            .collect::<Vec<_>>();
        server_roots.sort();
        server_roots.dedup();
        server_roots
    }

    async fn reopen_tracked_documents(&self) -> Result<()> {
        for document in self.document_tracker.open_documents() {
            let Some(client) = self.lsp_clients.get(&document.language_id) else {
                continue;
            };
            client
                .notify("textDocument/didOpen", document.did_open_params())
                .await?;
        }
        Ok(())
    }

    /// Shut down every language server owned by this translator.
    ///
    /// # Errors
    ///
    /// Returns the first language-server shutdown error after attempting to
    /// stop all owned servers.
    pub async fn shutdown(&mut self) -> Result<()> {
        let servers = std::mem::take(&mut self.lsp_servers);
        self.lsp_clients.clear();
        self.lsp_roots.clear();
        self.expected_languages.clear();

        shutdown_servers(servers).await
    }

    /// Add a linked workspace root, restarting active servers with all roots.
    ///
    /// # Errors
    ///
    /// Returns an error when active servers cannot be restarted for the
    /// expanded root set.
    pub async fn add_workspace_root(&mut self, root: PathBuf) -> Result<ProjectActivation> {
        if self.workspace_roots.contains(&root) {
            return Ok(ProjectActivation::ready());
        }
        let mut roots = self.workspace_roots.clone();
        roots.push(root);
        if self.lsp_clients.is_empty() || self.lsp_configs.is_empty() {
            self.set_workspace_roots(roots);
            return Ok(ProjectActivation::ready());
        }
        self.shutdown().await?;
        self.activate_project_with_roots(roots).await
    }

    /// Get the document tracker.
    #[must_use]
    pub const fn document_tracker(&self) -> &DocumentTracker {
        &self.document_tracker
    }

    /// Get a mutable reference to the document tracker.
    pub const fn document_tracker_mut(&mut self) -> &mut DocumentTracker {
        &mut self.document_tracker
    }

    /// Commit planned content to an open document and notify its LSP client.
    ///
    /// When no matching client is active, the actor-owned document tracker is
    /// still updated; a later activation will observe the new content.
    ///
    /// # Errors
    ///
    /// Returns an error when the document is not open, its version changed,
    /// or the LSP change notification cannot be delivered.
    pub async fn apply_open_document_content(
        &mut self,
        path: &Path,
        expected_version: i32,
        content: String,
    ) -> Result<()> {
        let Some(document) = self.document_tracker.get(path) else {
            return Err(Error::DocumentNotFound(path.to_path_buf()));
        };
        if document.version != expected_version {
            return Err(Error::InvalidToolParams(format!(
                "document version changed for {}: expected {}, got {}",
                path.display(),
                expected_version,
                document.version
            )));
        }
        let next_version = expected_version.saturating_add(1);
        if let Some(client) = self
            .lsp_clients
            .get(&detect_language(path, &self.extension_map))
            .cloned()
        {
            let params = DidChangeTextDocumentParams {
                text_document: VersionedTextDocumentIdentifier {
                    uri: document.uri.clone(),
                    version: next_version,
                },
                content_changes: vec![TextDocumentContentChangeEvent {
                    range: None,
                    range_length: None,
                    text: content.clone(),
                }],
            };
            client.notify("textDocument/didChange", params).await?;
        }
        self.document_tracker
            .update(path, content)
            .ok_or_else(|| Error::DocumentNotFound(path.to_path_buf()))?;
        Ok(())
    }

    /// Get the notification cache.
    #[must_use]
    pub const fn notification_cache(&self) -> &NotificationCache {
        &self.notification_cache
    }

    /// Get a mutable reference to the notification cache.
    pub const fn notification_cache_mut(&mut self) -> &mut NotificationCache {
        &mut self.notification_cache
    }

    // TODO: These methods will be implemented in Phase 3-5
    // Initialize and shutdown are now handled by LspServer in lifecycle.rs

    // Future implementation will use LspServer instead of LspClient directly
}

/// Add rust-analyzer's explicit linked-project manifests for a shared actor.
///
/// The registry only joins projects after a fail-closed compatibility check,
/// so a multi-root actor can safely give rust-analyzer every member manifest.
/// Existing initialization options are preserved and an existing array is
/// extended without duplicates.
fn linked_project_initialization_options(
    config: &LspServerConfig,
    roots: &[PathBuf],
) -> Result<Option<serde_json::Value>> {
    if roots.len() < 2 || !config.language_id.eq_ignore_ascii_case("rust") {
        return Ok(config.initialization_options.clone());
    }

    let mut options = match config.initialization_options.clone() {
        None => serde_json::Map::new(),
        Some(serde_json::Value::Object(options)) => options,
        Some(_) => {
            return Err(Error::InvalidConfig(
                "rust linked-project initialization_options must be a JSON object".to_string(),
            ));
        }
    };

    let mut linked_projects = take_linked_projects(&mut options)?;
    for manifest in cargo_manifest_values(roots)? {
        if !linked_projects.contains(&manifest) {
            linked_projects.push(manifest);
        }
    }
    options.insert(
        "linkedProjects".to_string(),
        serde_json::Value::Array(linked_projects),
    );
    Ok(Some(serde_json::Value::Object(options)))
}

fn take_linked_projects(
    options: &mut serde_json::Map<String, serde_json::Value>,
) -> Result<Vec<serde_json::Value>> {
    match options.remove("linkedProjects") {
        None => Ok(Vec::new()),
        Some(serde_json::Value::Array(projects)) => Ok(projects),
        Some(_) => Err(Error::InvalidConfig(
            "rust-analyzer linkedProjects must be a JSON array".to_string(),
        )),
    }
}

fn cargo_manifest_values(roots: &[PathBuf]) -> Result<Vec<serde_json::Value>> {
    roots
        .iter()
        .map(|root| {
            let manifest = root.join("Cargo.toml");
            if !manifest.is_file() {
                return Err(Error::InvalidConfig(format!(
                    "shared rust project has no Cargo.toml: {}",
                    root.display()
                )));
            }
            Ok(serde_json::Value::String(
                manifest.to_string_lossy().into_owned(),
            ))
        })
        .collect()
}

impl TranslatorTemplate {
    /// Build a fresh translator configured for one project root.
    #[must_use]
    pub fn translator_for_root(&self, root: PathBuf) -> Translator {
        let mut translator = Translator::new().with_extensions(self.extension_map.clone());
        translator.set_workspace_roots(vec![root]);
        translator.set_lsp_configs(self.lsp_configs.clone(), self.heuristics_max_depth);
        translator.set_redaction_patterns(self.redaction_patterns.clone());
        translator
    }
}

impl Default for Translator {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DiagnosticRequestParams {
    text_document: TextDocumentIdentifier,
    #[serde(skip_serializing_if = "Option::is_none")]
    identifier: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    previous_result_id: Option<String>,
    #[serde(flatten)]
    work_done_progress_params: WorkDoneProgressParams,
    #[serde(flatten)]
    partial_result_params: PartialResultParams,
}

fn diagnostic_request_params(text_document: TextDocumentIdentifier) -> DiagnosticRequestParams {
    DiagnosticRequestParams {
        text_document,
        identifier: None,
        previous_result_id: None,
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: PartialResultParams::default(),
    }
}

/// Position in a document (1-based for MCP).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Position2D {
    /// Line number (1-based).
    pub line: u32,
    /// Character offset (1-based).
    pub character: u32,
}

/// Range in a document (1-based for MCP).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Range {
    /// Start position.
    pub start: Position2D,
    /// End position.
    pub end: Position2D,
}

/// Location in a document.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Location {
    /// URI of the document.
    pub uri: String,
    /// Range within the document.
    pub range: Range,
}

/// Result of a hover request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HoverResult {
    /// Hover contents as markdown string.
    pub contents: String,
    /// Optional range the hover applies to.
    pub range: Option<Range>,
}

/// Result of a definition request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DefinitionResult {
    /// Locations of the definition.
    pub locations: Vec<Location>,
}

/// Result of a references request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReferencesResult {
    /// Locations of all references.
    pub locations: Vec<Location>,
}

/// Diagnostic severity.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DiagnosticSeverity {
    /// Error diagnostic.
    Error,
    /// Warning diagnostic.
    Warning,
    /// Informational diagnostic.
    Information,
    /// Hint diagnostic.
    Hint,
}

/// A single diagnostic.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Diagnostic {
    /// Range where the diagnostic applies.
    pub range: Range,
    /// Severity of the diagnostic.
    pub severity: DiagnosticSeverity,
    /// Diagnostic message.
    pub message: String,
    /// Optional diagnostic code.
    pub code: Option<String>,
}

/// Result of a diagnostics request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnosticsResult {
    /// List of diagnostics for the document.
    pub diagnostics: Vec<Diagnostic>,
}

/// A text edit operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextEdit {
    /// Range to replace.
    pub range: Range,
    /// New text.
    pub new_text: String,
}

/// Changes to a document.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocumentChanges {
    /// URI of the document.
    pub uri: String,
    /// Expected LSP document version, when supplied.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<i32>,
    /// List of edits to apply.
    pub edits: Vec<TextEdit>,
}

/// Result of a rename request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenameResult {
    /// Changes to apply across documents.
    pub changes: Vec<DocumentChanges>,
}

/// A completion item.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Completion {
    /// Label of the completion.
    pub label: String,
    /// Kind of completion.
    pub kind: Option<String>,
    /// Detail information.
    pub detail: Option<String>,
    /// Documentation.
    pub documentation: Option<String>,
}

/// Result of a completions request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletionsResult {
    /// List of completion items.
    pub items: Vec<Completion>,
}

/// A document symbol.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Symbol {
    /// Name of the symbol.
    pub name: String,
    /// Kind of symbol.
    pub kind: String,
    /// Range of the symbol.
    pub range: Range,
    /// Selection range (identifier location).
    pub selection_range: Range,
    /// Child symbols.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub children: Option<Vec<Self>>,
}

/// Result of a document symbols request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocumentSymbolsResult {
    /// List of symbols in the document.
    pub symbols: Vec<Symbol>,
}

/// Result of a format document request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FormatDocumentResult {
    /// List of edits to format the document.
    pub edits: Vec<TextEdit>,
}

/// A workspace symbol.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceSymbol {
    /// Name of the symbol.
    pub name: String,
    /// Kind of symbol.
    pub kind: String,
    /// Location of the symbol.
    pub location: Location,
    /// Optional container name (parent scope).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container_name: Option<String>,
}

/// Result of workspace symbol search.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceSymbolResult {
    /// List of symbols found.
    pub symbols: Vec<WorkspaceSymbol>,
}

/// A single code action.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeAction {
    /// Opaque project-scoped reference for previewing this action.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action_id: Option<String>,
    /// Title of the code action.
    pub title: String,
    /// Kind of code action (quickfix, refactor, etc.).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Diagnostics that this action resolves.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub diagnostics: Vec<Diagnostic>,
    /// Workspace edit to apply.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub edit: Option<WorkspaceEditDescription>,
    /// Lossless raw workspace edit, including document changes and resource operations.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_edit: Option<serde_json::Value>,
    /// Command to execute.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<CommandDescription>,
    /// Whether this is the preferred action.
    #[serde(default)]
    pub is_preferred: bool,
    /// LSP-disabled reason, when the action cannot currently run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disabled: Option<String>,
    /// Opaque LSP data used by `codeAction/resolve`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

/// Description of a workspace edit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceEditDescription {
    /// Changes to apply to documents.
    pub changes: Vec<DocumentChanges>,
}

/// Description of a command.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandDescription {
    /// Title of the command.
    pub title: String,
    /// Command identifier.
    pub command: String,
    /// Command arguments.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub arguments: Vec<serde_json::Value>,
}

/// Result of code actions request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeActionsResult {
    /// Available code actions.
    pub actions: Vec<CodeAction>,
}

/// A call hierarchy item.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallHierarchyItemResult {
    /// Name of the symbol.
    pub name: String,
    /// LSP numeric symbol kind (e.g. 12 for Function).
    pub kind: u32,
    /// More detail for this item.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// URI of the document.
    pub uri: String,
    /// Range of the symbol.
    pub range: Range,
    /// Selection range (identifier location).
    ///
    /// Serialized as `selectionRange` (camelCase) so that the value returned by
    /// `prepare_call_hierarchy` round-trips correctly when the MCP client passes
    /// it back to `get_incoming_calls` / `get_outgoing_calls`, which deserialize
    /// it as `lsp_types::CallHierarchyItem` (camelCase).
    #[serde(rename = "selectionRange")]
    pub selection_range: Range,
    /// Opaque data to pass to incoming/outgoing calls.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

/// Result of call hierarchy prepare request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallHierarchyPrepareResult {
    /// List of callable items at the position.
    pub items: Vec<CallHierarchyItemResult>,
}

/// An incoming call (caller of the current item).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IncomingCall {
    /// The item that calls the current item.
    pub from: CallHierarchyItemResult,
    /// Ranges where the call occurs.
    pub from_ranges: Vec<Range>,
}

/// Result of incoming calls request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IncomingCallsResult {
    /// List of incoming calls.
    pub calls: Vec<IncomingCall>,
}

/// An outgoing call (callee from the current item).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutgoingCall {
    /// The item being called.
    pub to: CallHierarchyItemResult,
    /// Ranges where the call occurs.
    pub from_ranges: Vec<Range>,
}

/// Result of outgoing calls request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutgoingCallsResult {
    /// List of outgoing calls.
    pub calls: Vec<OutgoingCall>,
}

/// Result of server logs request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerLogsResult {
    /// List of log entries.
    pub logs: Vec<crate::bridge::notifications::LogEntry>,
}

/// Result of server messages request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerMessagesResult {
    /// List of server messages.
    pub messages: Vec<crate::bridge::notifications::ServerMessage>,
}

/// Negotiated capabilities for one active language server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerCapability {
    /// Language ID configured for the server.
    pub language_id: String,
    /// Position encoding negotiated during initialization.
    pub position_encoding: String,
    /// Raw LSP server capabilities, with no environment or command details.
    pub capabilities: serde_json::Value,
}

/// A single parameter in a signature.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignatureParameter {
    /// Label of the parameter.
    pub label: String,
    /// Optional documentation for the parameter.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub documentation: Option<String>,
}

/// A single signature overload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignatureInfo {
    /// Full label of the signature.
    pub label: String,
    /// Optional documentation for the signature.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub documentation: Option<String>,
    /// Parameters of the signature.
    pub parameters: Vec<SignatureParameter>,
}

/// Result of a signature help request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignatureHelpResult {
    /// Available signatures.
    pub signatures: Vec<SignatureInfo>,
    /// Index of the active signature.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_signature: Option<u32>,
    /// Index of the active parameter within the active signature.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_parameter: Option<u32>,
}

/// Result of a go-to-implementation or go-to-type-definition request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocationsResult {
    /// Locations found.
    pub locations: Vec<Location>,
}

/// A single inlay hint entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InlayHintEntry {
    /// Position of the hint (1-based MCP).
    pub position: Position2D,
    /// Label text for the hint.
    pub label: String,
    /// Hint kind (1 = Type, 2 = Parameter).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<u8>,
    /// Whether to add a space before the hint.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub padding_left: Option<bool>,
    /// Whether to add a space after the hint.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub padding_right: Option<bool>,
    /// Tooltip text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tooltip: Option<String>,
}

/// Result of an inlay hints request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InlayHintsResult {
    /// List of inlay hints.
    pub hints: Vec<InlayHintEntry>,
}

/// Maximum allowed position value for validation.
const MAX_POSITION_VALUE: u32 = 1_000_000;
/// Maximum allowed range size in lines.
const MAX_RANGE_LINES: u32 = 10_000;

impl Translator {
    /// Validate that a path is within allowed workspace boundaries.
    ///
    /// # Errors
    ///
    /// Returns `Error::PathOutsideWorkspace` if the path is outside all workspace roots.
    pub(crate) fn validate_path(&self, path: &Path) -> Result<PathBuf> {
        let canonical = path.canonicalize().map_err(|e| Error::FileIo {
            path: path.to_path_buf(),
            source: e,
        })?;

        // If no workspace roots configured, allow any path (backward compatibility)
        if self.workspace_roots.is_empty() {
            return Ok(canonical);
        }

        // Check if path is within any workspace root
        for root in &self.workspace_roots {
            if let Ok(canonical_root) = root.canonicalize()
                && canonical.starts_with(&canonical_root)
            {
                return Ok(canonical);
            }
        }

        Err(Error::PathOutsideWorkspace(path.to_path_buf()))
    }

    /// Get a cloned LSP client for a file path based on language detection.
    fn get_client_for_file(&self, path: &Path) -> Result<LspClient> {
        let language_id = detect_language(path, &self.extension_map);
        self.lsp_clients.get(&language_id).cloned().ok_or_else(|| {
            // A configured+applicable language whose server has not registered
            // yet is still initializing (e.g. a large Unity solution loading via
            // OmniSharp); tell the caller to wait and retry rather than implying
            // no server is configured at all.
            if self.expected_languages.contains(&language_id) {
                Error::ServerInitializing(language_id)
            } else {
                Error::NoServerForLanguage(language_id)
            }
        })
    }

    fn registered_workspace_root(&self, path: &Path) -> Option<PathBuf> {
        crate::project::longest_matching_root(path, &self.workspace_roots).map(Path::to_path_buf)
    }

    fn has_lsp_root(&self, language_id: &str, root: &Path) -> bool {
        self.lsp_clients.contains_key(language_id)
            && self
                .lsp_roots
                .get(language_id)
                .is_some_and(|roots| roots.iter().any(|registered| registered == root))
    }

    fn project_root_for_file(&self, path: &Path, config: &LspServerConfig) -> Option<PathBuf> {
        // Prefer an active language-server root before the broader daemon
        // project root. A container project can own a nested Cargo package.
        if let Some(root) = self
            .lsp_roots
            .get(&config.language_id)
            .and_then(|roots| crate::project::longest_matching_root(path, roots))
        {
            return Some(root.to_path_buf());
        }

        // Resolve a registered daemon project through the same marker discovery
        // used by explicit activation. The outer root remains the routing
        // boundary, while the nested marker root is the LSP process root.
        if let Some(root) = self.registered_workspace_root(path) {
            return crate::project::longest_matching_root(
                path,
                &self.server_workspace_roots(config, std::slice::from_ref(&root)),
            )
            .map(Path::to_path_buf);
        }

        let start = path.parent().unwrap_or(path);

        if let Some(heuristics) = &config.heuristics
            && !heuristics.project_markers.is_empty()
        {
            for ancestor in start.ancestors() {
                if heuristics.is_applicable(ancestor) {
                    return Some(ancestor.to_path_buf());
                }
            }
            return None;
        }

        Some(start.to_path_buf())
    }

    fn wait_for_language_ready(&self, language_id: &str) -> Result<()> {
        if self.expected_languages.contains(language_id) {
            return Err(Error::ServerInitializing(language_id.to_string()));
        }
        Ok(())
    }

    fn workspace_symbol_clients(&self) -> Vec<LspClient> {
        let mut language_ids = self
            .lsp_servers
            .iter()
            .filter(|(_, server)| {
                server
                    .capabilities()
                    .workspace_symbol_provider
                    .as_ref()
                    .is_some_and(|provider| match provider {
                        lsp_types::OneOf::Left(enabled) => *enabled,
                        lsp_types::OneOf::Right(_) => true,
                    })
            })
            .filter_map(|(language_id, _)| {
                self.lsp_clients
                    .contains_key(language_id)
                    .then_some(language_id.as_str())
            })
            .collect::<Vec<_>>();
        language_ids.sort_unstable();
        language_ids
            .into_iter()
            .filter_map(|language_id| self.lsp_clients.get(language_id).cloned())
            .collect()
    }

    async fn ensure_client_for_file(&mut self, path: &Path) -> Result<()> {
        let language_id = detect_language(path, &self.extension_map);
        let Some(config) = self.lsp_configs.get(&language_id).cloned() else {
            return Ok(());
        };

        // The startup path may still be registering the configured server.
        // Let get_client_for_file return ServerInitializing instead of racing
        // it with a second process for the same language.
        if self.expected_languages.contains(&language_id)
            && !self.lsp_clients.contains_key(&language_id)
        {
            return Ok(());
        }

        self.wait_for_language_ready(&language_id)?;

        let Some(root) = self.project_root_for_file(path, &config) else {
            return Ok(());
        };

        if self.has_lsp_root(&language_id, &root) {
            return Ok(());
        }

        if let Some(server) = self.lsp_servers.remove(&language_id) {
            let _ = server.shutdown().await;
        }
        self.lsp_clients.remove(&language_id);
        self.lsp_roots.remove(&language_id);

        let mut server = LspServer::spawn(ServerInitConfig {
            server_config: config.clone(),
            workspace_roots: vec![root.clone()],
            initialization_options: config.initialization_options.clone(),
            notification_tx: None,
        })
        .await?;
        let client = server.client().clone();
        let _ = server.take_notification_rx();
        self.register_client(language_id.clone(), client);
        self.register_server(language_id.clone(), server);
        self.register_server_root(language_id, root);
        Ok(())
    }

    /// Parse and validate a file URI, returning the validated path.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The URI doesn't have a file:// scheme
    /// - The path is outside workspace boundaries
    fn parse_file_uri(&self, uri: &lsp_types::Uri) -> Result<PathBuf> {
        let uri_str = uri.as_str();

        // Validate file:// scheme
        if !uri_str.starts_with("file://") {
            return Err(Error::InvalidToolParams(format!(
                "Invalid URI scheme, expected file:// but got: {uri_str}"
            )));
        }

        // Extract path after file://
        let path_str = &uri_str["file://".len()..];

        // Handle Windows paths: file:///C:/path -> /C:/path -> C:/path
        // On Windows, URIs have format file:///C:/path, so we need to strip the leading /
        #[cfg(windows)]
        let path_str = if path_str.len() >= 3
            && path_str.starts_with('/')
            && path_str.chars().nth(2) == Some(':')
        {
            &path_str[1..]
        } else {
            path_str
        };

        let path = PathBuf::from(path_str);

        // Validate path is within workspace
        self.validate_path(&path)
    }

    /// Handle hover request.
    ///
    /// # Errors
    ///
    /// Returns an error if the LSP request fails or the file cannot be opened.
    pub async fn handle_hover(
        &mut self,
        file_path: String,
        line: u32,
        character: u32,
    ) -> Result<HoverResult> {
        let path = PathBuf::from(&file_path);
        let validated_path = self.validate_path(&path)?;
        self.ensure_client_for_file(&validated_path).await?;
        let client = self.get_client_for_file(&validated_path)?;
        let uri = self
            .document_tracker
            .ensure_open(&validated_path, &client)
            .await?;
        let lsp_position = mcp_to_lsp_position(line, character);

        let params = LspHoverParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                position: lsp_position,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
        };

        let timeout_duration = Duration::from_secs(30);
        let response: Option<Hover> = client
            .request("textDocument/hover", params, timeout_duration)
            .await?;

        let result = match response {
            Some(hover) => {
                let contents = extract_hover_contents(hover.contents);
                let range = hover.range.map(normalize_range);
                HoverResult { contents, range }
            }
            None => HoverResult {
                contents: "No hover information available".to_string(),
                range: None,
            },
        };

        Ok(result)
    }

    /// Handle definition request.
    ///
    /// # Errors
    ///
    /// Returns an error if the LSP request fails or the file cannot be opened.
    pub async fn handle_definition(
        &mut self,
        file_path: String,
        line: u32,
        character: u32,
    ) -> Result<DefinitionResult> {
        let path = PathBuf::from(&file_path);
        let validated_path = self.validate_path(&path)?;
        self.ensure_client_for_file(&validated_path).await?;
        let client = self.get_client_for_file(&validated_path)?;
        let uri = self
            .document_tracker
            .ensure_open(&validated_path, &client)
            .await?;
        let lsp_position = mcp_to_lsp_position(line, character);

        let params = GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
                position: lsp_position,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };

        let timeout_duration = Duration::from_secs(30);
        let response: Option<lsp_types::GotoDefinitionResponse> = client
            .request("textDocument/definition", params, timeout_duration)
            .await?;

        let locations = match response {
            Some(lsp_types::GotoDefinitionResponse::Scalar(loc)) => vec![loc],
            Some(lsp_types::GotoDefinitionResponse::Array(locs)) => locs,
            Some(lsp_types::GotoDefinitionResponse::Link(links)) => links
                .into_iter()
                .map(|link| lsp_types::Location {
                    uri: link.target_uri,
                    range: link.target_selection_range,
                })
                .collect(),
            None => vec![],
        };

        let result = DefinitionResult {
            locations: locations
                .into_iter()
                .map(|loc| Location {
                    uri: loc.uri.to_string(),
                    range: normalize_range(loc.range),
                })
                .collect(),
        };

        Ok(result)
    }

    /// Handle references request.
    ///
    /// # Errors
    ///
    /// Returns an error if the LSP request fails or the file cannot be opened.
    pub async fn handle_references(
        &mut self,
        file_path: String,
        line: u32,
        character: u32,
        include_declaration: bool,
    ) -> Result<ReferencesResult> {
        let path = PathBuf::from(&file_path);
        let validated_path = self.validate_path(&path)?;
        self.ensure_client_for_file(&validated_path).await?;
        let client = self.get_client_for_file(&validated_path)?;
        let uri = self
            .document_tracker
            .ensure_open(&validated_path, &client)
            .await?;
        let lsp_position = mcp_to_lsp_position(line, character);

        let params = ReferenceParams {
            text_document_position: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
                position: lsp_position,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
            context: ReferenceContext {
                include_declaration,
            },
        };

        let timeout_duration = Duration::from_secs(30);
        let response: Option<Vec<lsp_types::Location>> = client
            .request("textDocument/references", params, timeout_duration)
            .await?;

        let locations = response.unwrap_or_default();

        let result = ReferencesResult {
            locations: locations
                .into_iter()
                .map(|loc| Location {
                    uri: loc.uri.to_string(),
                    range: normalize_range(loc.range),
                })
                .collect(),
        };

        Ok(result)
    }

    /// Handle diagnostics request.
    ///
    /// # Errors
    ///
    /// Returns an error if the LSP request fails or the file cannot be opened.
    pub async fn handle_diagnostics(&mut self, file_path: String) -> Result<DiagnosticsResult> {
        let path = PathBuf::from(&file_path);
        let validated_path = self.validate_path(&path)?;
        self.ensure_client_for_file(&validated_path).await?;
        let language_id = detect_language(&validated_path, &self.extension_map);

        if self
            .lsp_servers
            .get(&language_id)
            .is_some_and(|server| server.capabilities().diagnostic_provider.is_none())
        {
            return self.handle_cached_diagnostics(&file_path);
        }

        let client = self.get_client_for_file(&validated_path)?;
        let uri = self
            .document_tracker
            .ensure_open(&validated_path, &client)
            .await?;

        let params = diagnostic_request_params(TextDocumentIdentifier { uri });

        let timeout_duration = Duration::from_secs(30);
        let response: lsp_types::DocumentDiagnosticReportResult = client
            .request("textDocument/diagnostic", params, timeout_duration)
            .await?;

        let diagnostics = match response {
            lsp_types::DocumentDiagnosticReportResult::Report(report) => match report {
                lsp_types::DocumentDiagnosticReport::Full(full) => {
                    full.full_document_diagnostic_report.items
                }
                lsp_types::DocumentDiagnosticReport::Unchanged(_) => vec![],
            },
            lsp_types::DocumentDiagnosticReportResult::Partial(_) => vec![],
        };

        let result = DiagnosticsResult {
            diagnostics: diagnostics
                .into_iter()
                .map(|diag| Diagnostic {
                    range: normalize_range(diag.range),
                    severity: match diag.severity {
                        Some(lsp_types::DiagnosticSeverity::ERROR) => DiagnosticSeverity::Error,
                        Some(lsp_types::DiagnosticSeverity::WARNING) => DiagnosticSeverity::Warning,
                        Some(lsp_types::DiagnosticSeverity::INFORMATION) => {
                            DiagnosticSeverity::Information
                        }
                        Some(lsp_types::DiagnosticSeverity::HINT) => DiagnosticSeverity::Hint,
                        _ => DiagnosticSeverity::Information,
                    },
                    message: diag.message,
                    code: diag.code.map(|c| match c {
                        lsp_types::NumberOrString::Number(n) => n.to_string(),
                        lsp_types::NumberOrString::String(s) => s,
                    }),
                })
                .collect(),
        };

        Ok(result)
    }

    /// Handle rename request.
    ///
    /// # Errors
    ///
    /// Returns an error if the LSP request fails or the file cannot be opened.
    pub async fn handle_rename(
        &mut self,
        file_path: String,
        line: u32,
        character: u32,
        new_name: String,
    ) -> Result<RenameResult> {
        let edit = self
            .request_rename_workspace_edit(file_path, line, character, new_name)
            .await?;
        let changes = edit.map_or_else(|| Ok(Vec::new()), workspace_edit_document_changes)?;

        Ok(RenameResult { changes })
    }

    /// Request the raw LSP workspace edit for a rename.
    ///
    /// This is kept separate from the legacy rename DTO so callers can feed
    /// the edit through the generic preview/apply engine without losing
    /// cross-file or resource operations.
    ///
    /// # Errors
    ///
    /// Returns an error when the file is outside the workspace, no applicable
    /// language server is available, or the LSP request fails.
    pub async fn request_rename_workspace_edit(
        &mut self,
        file_path: String,
        line: u32,
        character: u32,
        new_name: String,
    ) -> Result<Option<WorkspaceEdit>> {
        let path = PathBuf::from(&file_path);
        let validated_path = self.validate_path(&path)?;
        self.ensure_client_for_file(&validated_path).await?;
        let client = self.get_client_for_file(&validated_path)?;
        let uri = self
            .document_tracker
            .ensure_open(&validated_path, &client)
            .await?;
        let lsp_position = mcp_to_lsp_position(line, character);

        let params = LspRenameParams {
            text_document_position: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
                position: lsp_position,
            },
            new_name,
            work_done_progress_params: WorkDoneProgressParams::default(),
        };

        let timeout_duration = Duration::from_secs(30);
        client
            .request("textDocument/rename", params, timeout_duration)
            .await
    }

    /// Handle completions request.
    ///
    /// # Errors
    ///
    /// Returns an error if the LSP request fails or the file cannot be opened.
    pub async fn handle_completions(
        &mut self,
        file_path: String,
        line: u32,
        character: u32,
        trigger: Option<String>,
    ) -> Result<CompletionsResult> {
        let path = PathBuf::from(&file_path);
        let validated_path = self.validate_path(&path)?;
        self.ensure_client_for_file(&validated_path).await?;
        let client = self.get_client_for_file(&validated_path)?;
        let uri = self
            .document_tracker
            .ensure_open(&validated_path, &client)
            .await?;
        let lsp_position = mcp_to_lsp_position(line, character);

        let context = trigger.map(|trigger_char| lsp_types::CompletionContext {
            trigger_kind: CompletionTriggerKind::TRIGGER_CHARACTER,
            trigger_character: Some(trigger_char),
        });

        let params = CompletionParams {
            text_document_position: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
                position: lsp_position,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
            context,
        };

        let timeout_duration = Duration::from_secs(10);
        let response: Option<lsp_types::CompletionResponse> = client
            .request("textDocument/completion", params, timeout_duration)
            .await?;

        let items = match response {
            Some(lsp_types::CompletionResponse::Array(items)) => items,
            Some(lsp_types::CompletionResponse::List(list)) => list.items,
            None => vec![],
        };

        let result = CompletionsResult {
            items: items
                .into_iter()
                .map(|item| Completion {
                    label: item.label,
                    kind: item.kind.map(|k| format!("{k:?}")),
                    detail: item.detail,
                    documentation: item.documentation.map(|doc| match doc {
                        lsp_types::Documentation::String(s) => s,
                        lsp_types::Documentation::MarkupContent(m) => m.value,
                    }),
                })
                .collect(),
        };

        Ok(result)
    }

    /// Handle document symbols request.
    ///
    /// # Errors
    ///
    /// Returns an error if the LSP request fails or the file cannot be opened.
    pub async fn handle_document_symbols(
        &mut self,
        file_path: String,
    ) -> Result<DocumentSymbolsResult> {
        let path = PathBuf::from(&file_path);
        let validated_path = self.validate_path(&path)?;
        self.ensure_client_for_file(&validated_path).await?;
        let client = self.get_client_for_file(&validated_path)?;
        let uri = self
            .document_tracker
            .ensure_open(&validated_path, &client)
            .await?;

        let params = DocumentSymbolParams {
            text_document: TextDocumentIdentifier { uri: uri.clone() },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };

        let timeout_duration = Duration::from_secs(30);
        let response: Option<lsp_types::DocumentSymbolResponse> = client
            .request("textDocument/documentSymbol", params, timeout_duration)
            .await?;

        let symbols = match response {
            Some(lsp_types::DocumentSymbolResponse::Flat(symbols)) => symbols
                .into_iter()
                .map(|sym| Symbol {
                    name: sym.name,
                    kind: format!("{:?}", sym.kind),
                    range: normalize_range(sym.location.range),
                    selection_range: normalize_range(sym.location.range),
                    children: None,
                })
                .collect(),
            Some(lsp_types::DocumentSymbolResponse::Nested(symbols)) => {
                symbols.into_iter().map(convert_document_symbol).collect()
            }
            None => vec![],
        };

        Ok(DocumentSymbolsResult { symbols })
    }

    /// Handle format document request.
    ///
    /// # Errors
    ///
    /// Returns an error if the LSP request fails or the file cannot be opened.
    pub async fn handle_format_document(
        &mut self,
        file_path: String,
        tab_size: u32,
        insert_spaces: bool,
    ) -> Result<FormatDocumentResult> {
        let edit = self
            .request_format_workspace_edit(file_path, tab_size, insert_spaces)
            .await?;
        let edits = edit.map_or_else(Vec::new, |edit| {
            edit.changes.map_or_else(Vec::new, |changes| {
                changes
                    .into_values()
                    .flatten()
                    .map(|edit| TextEdit {
                        range: normalize_range(edit.range),
                        new_text: edit.new_text,
                    })
                    .collect()
            })
        });

        Ok(FormatDocumentResult { edits })
    }

    /// Request the raw LSP workspace edit for document formatting.
    ///
    /// # Errors
    ///
    /// Returns an error when the file is outside the workspace, no applicable
    /// language server is available, or the LSP request fails.
    pub async fn request_format_workspace_edit(
        &mut self,
        file_path: String,
        tab_size: u32,
        insert_spaces: bool,
    ) -> Result<Option<WorkspaceEdit>> {
        let path = PathBuf::from(&file_path);
        let validated_path = self.validate_path(&path)?;
        self.ensure_client_for_file(&validated_path).await?;
        let client = self.get_client_for_file(&validated_path)?;
        let uri = self
            .document_tracker
            .ensure_open(&validated_path, &client)
            .await?;

        let params = DocumentFormattingParams {
            text_document: TextDocumentIdentifier { uri: uri.clone() },
            options: FormattingOptions {
                tab_size,
                insert_spaces,
                ..Default::default()
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
        };

        let timeout_duration = Duration::from_secs(30);
        let response: Option<Vec<lsp_types::TextEdit>> = client
            .request("textDocument/formatting", params, timeout_duration)
            .await?;
        Ok(response.map(|edits| WorkspaceEdit {
            changes: Some(std::iter::once((uri, edits)).collect()),
            document_changes: None,
            change_annotations: None,
        }))
    }

    /// Handle workspace symbol search.
    ///
    /// # Errors
    ///
    /// Returns an error if the LSP request fails or no server is configured.
    pub async fn handle_workspace_symbol(
        &mut self,
        query: String,
        kind_filter: Option<String>,
        limit: u32,
    ) -> Result<WorkspaceSymbolResult> {
        const MAX_QUERY_LENGTH: usize = 1000;
        const VALID_SYMBOL_KINDS: &[&str] = &[
            "File",
            "Module",
            "Namespace",
            "Package",
            "Class",
            "Method",
            "Property",
            "Field",
            "Constructor",
            "Enum",
            "Interface",
            "Function",
            "Variable",
            "Constant",
            "String",
            "Number",
            "Boolean",
            "Array",
            "Object",
            "Key",
            "Null",
            "EnumMember",
            "Struct",
            "Event",
            "Operator",
            "TypeParameter",
        ];

        // Validate query length
        if query.len() > MAX_QUERY_LENGTH {
            return Err(Error::InvalidToolParams(format!(
                "Query too long: {} chars (max {MAX_QUERY_LENGTH})",
                query.len()
            )));
        }

        // Validate kind filter
        if let Some(ref kind) = kind_filter
            && !VALID_SYMBOL_KINDS
                .iter()
                .any(|k| k.eq_ignore_ascii_case(kind))
        {
            return Err(Error::InvalidToolParams(format!(
                "Invalid kind_filter: '{kind}'. Valid values: {VALID_SYMBOL_KINDS:?}"
            )));
        }

        // Workspace search requires at least one active LSP client. If none are
        // registered yet but a configured server is still initializing, tell the
        // caller to wait and retry rather than implying nothing is configured.
        if self.lsp_clients.is_empty() {
            return Err(self
                .expected_languages
                .iter()
                .next()
                .map_or(Error::NoServerConfigured, |lang| {
                    Error::ServerInitializing(lang.clone())
                }));
        }
        if limit == 0 {
            return Ok(WorkspaceSymbolResult {
                symbols: Vec::new(),
            });
        }

        let timeout_duration = Duration::from_secs(30);
        let mut symbols = Vec::new();
        for client in self.workspace_symbol_clients() {
            let params = LspWorkspaceSymbolParams {
                query: query.clone(),
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
            };
            let response: Option<Vec<lsp_types::SymbolInformation>> = client
                .request("workspace/symbol", params, timeout_duration)
                .await?;
            symbols.extend(
                response
                    .unwrap_or_default()
                    .into_iter()
                    .map(|sym| WorkspaceSymbol {
                        name: sym.name,
                        kind: format!("{:?}", sym.kind),
                        location: Location {
                            uri: sym.location.uri.to_string(),
                            range: normalize_range(sym.location.range),
                        },
                        container_name: sym.container_name,
                    })
                    .filter(|symbol| {
                        kind_filter
                            .as_ref()
                            .is_none_or(|kind| symbol.kind.eq_ignore_ascii_case(kind))
                    })
                    .take((limit as usize).saturating_sub(symbols.len())),
            );
            if symbols.len() == limit as usize {
                break;
            }
        }

        Ok(WorkspaceSymbolResult { symbols })
    }

    /// Handle code actions request.
    ///
    /// # Errors
    ///
    /// Returns an error if the LSP request fails or the file cannot be opened.
    pub async fn handle_code_actions(
        &mut self,
        file_path: String,
        start_line: u32,
        start_character: u32,
        end_line: u32,
        end_character: u32,
        kind_filter: Option<String>,
    ) -> Result<CodeActionsResult> {
        let response_vec = self
            .request_code_actions(
                file_path,
                start_line,
                start_character,
                end_line,
                end_character,
                kind_filter,
            )
            .await?;
        let mut actions = Vec::with_capacity(response_vec.len());

        for action_or_command in response_vec {
            let action = convert_code_action_or_command(action_or_command, None);
            actions.push(action);
        }

        Ok(CodeActionsResult { actions })
    }

    /// Request raw code actions for a range so an actor can retain action data
    /// for a later resolve/preview operation.
    ///
    /// # Errors
    ///
    /// Returns an error when the file is outside the workspace or the LSP
    /// request fails.
    pub async fn request_code_actions(
        &mut self,
        file_path: String,
        start_line: u32,
        start_character: u32,
        end_line: u32,
        end_character: u32,
        kind_filter: Option<String>,
    ) -> Result<Vec<lsp_types::CodeActionOrCommand>> {
        validate_code_action_params(
            start_line,
            start_character,
            end_line,
            end_character,
            kind_filter.as_deref(),
        )?;
        let path = PathBuf::from(&file_path);
        let validated_path = self.validate_path(&path)?;
        self.ensure_client_for_file(&validated_path).await?;
        let client = self.get_client_for_file(&validated_path)?;
        let uri = self
            .document_tracker
            .ensure_open(&validated_path, &client)
            .await?;
        let params = lsp_types::CodeActionParams {
            text_document: TextDocumentIdentifier { uri },
            range: lsp_types::Range {
                start: mcp_to_lsp_position(start_line, start_character),
                end: mcp_to_lsp_position(end_line, end_character),
            },
            context: lsp_types::CodeActionContext {
                diagnostics: Vec::new(),
                only: kind_filter.map(|kind| vec![lsp_types::CodeActionKind::from(kind)]),
                trigger_kind: Some(lsp_types::CodeActionTriggerKind::INVOKED),
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };
        let response: Option<lsp_types::CodeActionResponse> = client
            .request("textDocument/codeAction", params, Duration::from_secs(30))
            .await?;
        Ok(response.unwrap_or_default())
    }

    /// Resolve one raw code action through `codeAction/resolve`.
    ///
    /// # Errors
    ///
    /// Returns an error when the file is outside the workspace or the LSP
    /// server rejects the resolve request.
    pub async fn resolve_code_action(
        &mut self,
        file_path: &str,
        action: lsp_types::CodeAction,
    ) -> Result<lsp_types::CodeAction> {
        let validated_path = self.validate_path(Path::new(file_path))?;
        self.ensure_client_for_file(&validated_path).await?;
        let client = self.get_client_for_file(&validated_path)?;
        client
            .request("codeAction/resolve", action, Duration::from_secs(30))
            .await
    }

    /// Handle call hierarchy prepare request.
    ///
    /// # Errors
    ///
    /// Returns an error if the LSP request fails or the file cannot be opened.
    pub async fn handle_call_hierarchy_prepare(
        &mut self,
        file_path: String,
        line: u32,
        character: u32,
    ) -> Result<CallHierarchyPrepareResult> {
        // Validate position bounds
        if line < 1 || character < 1 {
            return Err(Error::InvalidToolParams(
                "Line and character positions must be >= 1".to_string(),
            ));
        }

        if line > MAX_POSITION_VALUE || character > MAX_POSITION_VALUE {
            return Err(Error::InvalidToolParams(format!(
                "Position values must be <= {MAX_POSITION_VALUE}"
            )));
        }

        let path = PathBuf::from(&file_path);
        let validated_path = self.validate_path(&path)?;
        self.ensure_client_for_file(&validated_path).await?;
        let client = self.get_client_for_file(&validated_path)?;
        let uri = self
            .document_tracker
            .ensure_open(&validated_path, &client)
            .await?;
        let lsp_position = mcp_to_lsp_position(line, character);

        let params = LspCallHierarchyPrepareParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
                position: lsp_position,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
        };

        let timeout_duration = Duration::from_secs(30);
        let response: Option<Vec<CallHierarchyItem>> = client
            .request(
                "textDocument/prepareCallHierarchy",
                params,
                timeout_duration,
            )
            .await?;

        // Pre-allocate and build result
        let lsp_items = response.unwrap_or_default();
        let mut items = Vec::with_capacity(lsp_items.len());
        for item in lsp_items {
            items.push(convert_call_hierarchy_item(item));
        }

        Ok(CallHierarchyPrepareResult { items })
    }

    /// Handle incoming calls request.
    ///
    /// # Errors
    ///
    /// Returns an error if the LSP request fails or the item is invalid.
    pub async fn handle_incoming_calls(
        &mut self,
        item: serde_json::Value,
    ) -> Result<IncomingCallsResult> {
        // Deserialize as our own type (1-based coords) then convert to LSP (0-based).
        let lsp_item = mcp_item_to_lsp(item)?;

        // Parse and validate the URI
        let path = self.parse_file_uri(&lsp_item.uri)?;
        self.ensure_client_for_file(&path).await?;
        let client = self.get_client_for_file(&path)?;

        let params = CallHierarchyIncomingCallsParams {
            item: lsp_item,
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };

        let timeout_duration = Duration::from_secs(30);
        let response: Option<Vec<CallHierarchyIncomingCall>> = client
            .request("callHierarchy/incomingCalls", params, timeout_duration)
            .await?;

        // Pre-allocate and build result
        let lsp_calls = response.unwrap_or_default();
        let mut calls = Vec::with_capacity(lsp_calls.len());

        for call in lsp_calls {
            let from_ranges = {
                let mut ranges = Vec::with_capacity(call.from_ranges.len());
                for range in call.from_ranges {
                    ranges.push(normalize_range(range));
                }
                ranges
            };

            calls.push(IncomingCall {
                from: convert_call_hierarchy_item(call.from),
                from_ranges,
            });
        }

        Ok(IncomingCallsResult { calls })
    }

    /// Handle outgoing calls request.
    ///
    /// # Errors
    ///
    /// Returns an error if the LSP request fails or the item is invalid.
    pub async fn handle_outgoing_calls(
        &mut self,
        item: serde_json::Value,
    ) -> Result<OutgoingCallsResult> {
        // Deserialize as our own type (1-based coords) then convert to LSP (0-based).
        let lsp_item = mcp_item_to_lsp(item)?;

        // Parse and validate the URI
        let path = self.parse_file_uri(&lsp_item.uri)?;
        self.ensure_client_for_file(&path).await?;
        let client = self.get_client_for_file(&path)?;

        let params = CallHierarchyOutgoingCallsParams {
            item: lsp_item,
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };

        let timeout_duration = Duration::from_secs(30);
        let response: Option<Vec<CallHierarchyOutgoingCall>> = client
            .request("callHierarchy/outgoingCalls", params, timeout_duration)
            .await?;

        // Pre-allocate and build result
        let lsp_calls = response.unwrap_or_default();
        let mut calls = Vec::with_capacity(lsp_calls.len());

        for call in lsp_calls {
            let from_ranges = {
                let mut ranges = Vec::with_capacity(call.from_ranges.len());
                for range in call.from_ranges {
                    ranges.push(normalize_range(range));
                }
                ranges
            };

            calls.push(OutgoingCall {
                to: convert_call_hierarchy_item(call.to),
                from_ranges,
            });
        }

        Ok(OutgoingCallsResult { calls })
    }

    /// Handle cached diagnostics request.
    ///
    /// # Errors
    ///
    /// Returns an error if the path is invalid or outside workspace boundaries.
    pub fn handle_cached_diagnostics(&mut self, file_path: &str) -> Result<DiagnosticsResult> {
        let uri = self.diagnostic_uri(file_path)?;

        let diagnostics =
            self.notification_cache
                .get_diagnostics(&uri)
                .map_or_else(Vec::new, |diag_info| {
                    diag_info
                        .diagnostics
                        .iter()
                        .map(|diag| Diagnostic {
                            range: normalize_range(diag.range),
                            severity: match diag.severity {
                                Some(lsp_types::DiagnosticSeverity::ERROR) => {
                                    DiagnosticSeverity::Error
                                }
                                Some(lsp_types::DiagnosticSeverity::WARNING) => {
                                    DiagnosticSeverity::Warning
                                }
                                Some(lsp_types::DiagnosticSeverity::INFORMATION) => {
                                    DiagnosticSeverity::Information
                                }
                                Some(lsp_types::DiagnosticSeverity::HINT) => {
                                    DiagnosticSeverity::Hint
                                }
                                _ => DiagnosticSeverity::Information,
                            },
                            message: diag.message.clone(),
                            code: diag.code.as_ref().map(|c| match c {
                                lsp_types::NumberOrString::Number(n) => n.to_string(),
                                lsp_types::NumberOrString::String(s) => s.clone(),
                            }),
                        })
                        .collect()
                });

        Ok(DiagnosticsResult { diagnostics })
    }

    /// Return whether diagnostics have been cached for a document path.
    ///
    /// # Errors
    ///
    /// Returns an error when the path cannot be validated against the
    /// translator's workspace roots.
    pub fn has_cached_diagnostics(&self, file_path: &str) -> Result<bool> {
        let uri = self.diagnostic_uri(file_path)?;
        Ok(self.notification_cache.contains_diagnostics(&uri))
    }

    fn diagnostic_uri(&self, file_path: &str) -> Result<String> {
        let path = PathBuf::from(file_path);
        let validated_path = self.validate_path(&path)?;
        // Use path_to_uri (strips \\?\ on Windows) so the key matches what
        // rust-analyzer stores in publishDiagnostics notifications.
        Ok(path_to_uri(&validated_path).to_string())
    }

    /// Handle server logs request.
    ///
    /// # Errors
    ///
    /// Returns an error if the `min_level` parameter is invalid.
    pub fn handle_server_logs(
        &mut self,
        limit: usize,
        min_level: Option<String>,
    ) -> Result<ServerLogsResult> {
        use crate::bridge::notifications::LogLevel;

        let min_level_filter = if let Some(level_str) = min_level {
            let level = match level_str.to_lowercase().as_str() {
                "error" => LogLevel::Error,
                "warning" => LogLevel::Warning,
                "info" => LogLevel::Info,
                "debug" => LogLevel::Debug,
                _ => {
                    return Err(Error::InvalidToolParams(format!(
                        "Invalid min_level: '{level_str}'. Valid values: error, warning, info, debug"
                    )));
                }
            };
            Some(level)
        } else {
            None
        };

        let all_logs = self.notification_cache.get_logs();

        let logs: Vec<_> = all_logs
            .iter()
            .filter(|log| {
                min_level_filter.is_none_or(|min| match min {
                    LogLevel::Error => matches!(log.level, LogLevel::Error),
                    LogLevel::Warning => matches!(log.level, LogLevel::Error | LogLevel::Warning),
                    LogLevel::Info => !matches!(log.level, LogLevel::Debug),
                    LogLevel::Debug => true,
                })
            })
            .take(limit)
            .cloned()
            .map(|mut log| {
                log.message = self.redaction_policy.redact(&log.message);
                log
            })
            .collect();

        Ok(ServerLogsResult { logs })
    }

    /// Handle server messages request.
    ///
    /// # Errors
    ///
    /// This method does not return errors.
    pub fn handle_server_messages(&mut self, limit: usize) -> Result<ServerMessagesResult> {
        let all_messages = self.notification_cache.get_messages();
        let messages: Vec<_> = all_messages
            .iter()
            .take(limit)
            .cloned()
            .map(|mut message| {
                message.message = self.redaction_policy.redact(&message.message);
                message
            })
            .collect();
        Ok(ServerMessagesResult { messages })
    }

    /// Handle signature help request (`textDocument/signatureHelp`).
    ///
    /// Returns parameter signatures and documentation while typing a function call.
    /// `context` is omitted (None) — the server infers trigger state from position.
    ///
    /// # Errors
    ///
    /// Returns an error if the LSP request fails or the file cannot be opened.
    pub async fn handle_signature_help(
        &mut self,
        file_path: String,
        line: u32,
        character: u32,
    ) -> Result<SignatureHelpResult> {
        let path = PathBuf::from(&file_path);
        let validated_path = self.validate_path(&path)?;
        self.ensure_client_for_file(&validated_path).await?;
        let client = self.get_client_for_file(&validated_path)?;
        let uri = self
            .document_tracker
            .ensure_open(&validated_path, &client)
            .await?;
        let lsp_position = mcp_to_lsp_position(line, character);

        let params = LspSignatureHelpParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
                position: lsp_position,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            context: None,
        };

        let timeout_duration = Duration::from_secs(30);
        let response: Option<lsp_types::SignatureHelp> = client
            .request("textDocument/signatureHelp", params, timeout_duration)
            .await?;

        let result = match response {
            Some(sig_help) => SignatureHelpResult {
                signatures: sig_help
                    .signatures
                    .into_iter()
                    .map(|sig| SignatureInfo {
                        label: sig.label,
                        documentation: sig.documentation.map(extract_documentation),
                        parameters: sig
                            .parameters
                            .unwrap_or_default()
                            .into_iter()
                            .map(|p| SignatureParameter {
                                label: match p.label {
                                    lsp_types::ParameterLabel::Simple(s) => s,
                                    lsp_types::ParameterLabel::LabelOffsets([start, end]) => {
                                        format!("[{start},{end}]")
                                    }
                                },
                                documentation: p.documentation.map(extract_documentation),
                            })
                            .collect(),
                    })
                    .collect(),
                active_signature: sig_help.active_signature,
                active_parameter: sig_help.active_parameter,
            },
            None => SignatureHelpResult {
                signatures: vec![],
                active_signature: None,
                active_parameter: None,
            },
        };

        Ok(result)
    }

    /// Handle go-to-implementation request (`textDocument/implementation`).
    ///
    /// Returns the locations of trait method or interface member implementations.
    ///
    /// # Errors
    ///
    /// Returns an error if the LSP request fails or the file cannot be opened.
    pub async fn handle_implementation(
        &mut self,
        file_path: String,
        line: u32,
        character: u32,
    ) -> Result<LocationsResult> {
        let path = PathBuf::from(&file_path);
        let validated_path = self.validate_path(&path)?;
        self.ensure_client_for_file(&validated_path).await?;
        let client = self.get_client_for_file(&validated_path)?;
        let uri = self
            .document_tracker
            .ensure_open(&validated_path, &client)
            .await?;
        let lsp_position = mcp_to_lsp_position(line, character);

        let params = GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
                position: lsp_position,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };

        let timeout_duration = Duration::from_secs(30);
        let response: Option<lsp_types::GotoDefinitionResponse> = client
            .request("textDocument/implementation", params, timeout_duration)
            .await?;

        Ok(LocationsResult {
            locations: goto_response_to_locations(response),
        })
    }

    /// Handle go-to-type-definition request (`textDocument/typeDefinition`).
    ///
    /// Returns the type definition location of the expression at position. Distinct
    /// from go-to-definition for variable bindings where definition and type differ.
    ///
    /// # Errors
    ///
    /// Returns an error if the LSP request fails or the file cannot be opened.
    pub async fn handle_type_definition(
        &mut self,
        file_path: String,
        line: u32,
        character: u32,
    ) -> Result<LocationsResult> {
        let path = PathBuf::from(&file_path);
        let validated_path = self.validate_path(&path)?;
        self.ensure_client_for_file(&validated_path).await?;
        let client = self.get_client_for_file(&validated_path)?;
        let uri = self
            .document_tracker
            .ensure_open(&validated_path, &client)
            .await?;
        let lsp_position = mcp_to_lsp_position(line, character);

        let params = GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
                position: lsp_position,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };

        let timeout_duration = Duration::from_secs(30);
        let response: Option<lsp_types::GotoDefinitionResponse> = client
            .request("textDocument/typeDefinition", params, timeout_duration)
            .await?;

        Ok(LocationsResult {
            locations: goto_response_to_locations(response),
        })
    }

    /// Handle inlay hints request (`textDocument/inlayHint`).
    ///
    /// Returns inferred type and parameter annotations the editor would render inline.
    /// Output positions are in MCP 1-based form.
    ///
    /// # Errors
    ///
    /// Returns an error if the LSP request fails or the file cannot be opened.
    pub async fn handle_inlay_hints(
        &mut self,
        file_path: String,
        start_line: u32,
        start_character: u32,
        end_line: u32,
        end_character: u32,
    ) -> Result<InlayHintsResult> {
        use crate::bridge::encoding::lsp_to_mcp_position;

        let path = PathBuf::from(&file_path);
        let validated_path = self.validate_path(&path)?;
        self.ensure_client_for_file(&validated_path).await?;
        let client = self.get_client_for_file(&validated_path)?;
        let uri = self
            .document_tracker
            .ensure_open(&validated_path, &client)
            .await?;

        let lsp_start = mcp_to_lsp_position(start_line, start_character);
        let lsp_end = mcp_to_lsp_position(end_line, end_character);

        let params = InlayHintParams {
            text_document: TextDocumentIdentifier { uri },
            range: lsp_types::Range {
                start: lsp_start,
                end: lsp_end,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
        };

        let timeout_duration = Duration::from_secs(30);
        let response: Option<Vec<lsp_types::InlayHint>> = client
            .request("textDocument/inlayHint", params, timeout_duration)
            .await?;

        let hints = response
            .unwrap_or_default()
            .into_iter()
            .map(|hint| {
                let (mcp_line, mcp_character) = lsp_to_mcp_position(hint.position);
                let label = match hint.label {
                    InlayHintLabel::String(s) => s,
                    InlayHintLabel::LabelParts(parts) => parts
                        .into_iter()
                        .map(|p| p.value)
                        .collect::<Vec<_>>()
                        .concat(),
                };
                let tooltip = hint.tooltip.map(|t| match t {
                    lsp_types::InlayHintTooltip::String(s) => s,
                    lsp_types::InlayHintTooltip::MarkupContent(m) => m.value,
                });
                InlayHintEntry {
                    position: Position2D {
                        line: mcp_line,
                        character: mcp_character,
                    },
                    label,
                    kind: hint.kind.and_then(|k| {
                        serde_json::to_value(k)
                            .ok()
                            .and_then(|v| v.as_i64())
                            .and_then(|n| u8::try_from(n).ok())
                    }),
                    padding_left: hint.padding_left,
                    padding_right: hint.padding_right,
                    tooltip,
                }
            })
            .collect();

        Ok(InlayHintsResult { hints })
    }
}

/// Extract hover contents as markdown string.
/// Convert LSP `Documentation` to a plain string.
fn extract_documentation(doc: lsp_types::Documentation) -> String {
    match doc {
        lsp_types::Documentation::String(s) => s,
        lsp_types::Documentation::MarkupContent(m) => m.value,
    }
}

/// Normalize a `GotoDefinitionResponse` into a flat list of MCP `Location` values.
fn goto_response_to_locations(
    response: Option<lsp_types::GotoDefinitionResponse>,
) -> Vec<Location> {
    let lsp_locs: Vec<lsp_types::Location> = match response {
        Some(lsp_types::GotoDefinitionResponse::Scalar(loc)) => vec![loc],
        Some(lsp_types::GotoDefinitionResponse::Array(locs)) => locs,
        Some(lsp_types::GotoDefinitionResponse::Link(links)) => links
            .into_iter()
            .map(|link| lsp_types::Location {
                uri: link.target_uri,
                range: link.target_selection_range,
            })
            .collect(),
        None => vec![],
    };

    lsp_locs
        .into_iter()
        .map(|loc| Location {
            uri: loc.uri.to_string(),
            range: normalize_range(loc.range),
        })
        .collect()
}

fn extract_hover_contents(contents: HoverContents) -> String {
    match contents {
        HoverContents::Scalar(marked_string) => marked_string_to_string(marked_string),
        HoverContents::Array(marked_strings) => marked_strings
            .into_iter()
            .map(marked_string_to_string)
            .collect::<Vec<_>>()
            .join("\n\n"),
        HoverContents::Markup(markup) => markup.value,
    }
}

/// Convert a marked string to a plain string.
fn marked_string_to_string(marked: MarkedString) -> String {
    match marked {
        MarkedString::String(s) => s,
        MarkedString::LanguageString(ls) => format!("```{}\n{}\n```", ls.language, ls.value),
    }
}

/// Convert LSP range to MCP range (0-based to 1-based).
/// Validate parameters for `handle_code_actions`.
fn validate_code_action_params(
    start_line: u32,
    start_character: u32,
    end_line: u32,
    end_character: u32,
    kind_filter: Option<&str>,
) -> Result<()> {
    const VALID_ACTION_KINDS: &[&str] = &[
        "quickfix",
        "refactor",
        "refactor.extract",
        "refactor.inline",
        "refactor.rewrite",
        "source",
        "source.organizeImports",
    ];

    if let Some(kind) = kind_filter
        && !VALID_ACTION_KINDS
            .iter()
            .any(|k| k.eq_ignore_ascii_case(kind))
    {
        return Err(Error::InvalidToolParams(format!(
            "Invalid kind_filter: '{kind}'. Valid values: {VALID_ACTION_KINDS:?}"
        )));
    }

    if start_line < 1 || start_character < 1 || end_line < 1 || end_character < 1 {
        return Err(Error::InvalidToolParams(
            "Line and character positions must be >= 1".to_string(),
        ));
    }

    if start_line > MAX_POSITION_VALUE
        || start_character > MAX_POSITION_VALUE
        || end_line > MAX_POSITION_VALUE
        || end_character > MAX_POSITION_VALUE
    {
        return Err(Error::InvalidToolParams(format!(
            "Position values must be <= {MAX_POSITION_VALUE}"
        )));
    }

    if end_line.saturating_sub(start_line) > MAX_RANGE_LINES {
        return Err(Error::InvalidToolParams(format!(
            "Range size must be <= {MAX_RANGE_LINES} lines"
        )));
    }

    if start_line > end_line || (start_line == end_line && start_character > end_character) {
        return Err(Error::InvalidToolParams(
            "Start position must be before or equal to end position".to_string(),
        ));
    }

    Ok(())
}

/// Convert a `CallHierarchyItemResult` JSON (1-based MCP coordinates) into
/// a `lsp_types::CallHierarchyItem` (0-based LSP coordinates).
///
/// MCP clients receive `CallHierarchyItemResult` from `prepare_call_hierarchy`
/// and pass it back opaquely to `get_incoming_calls` / `get_outgoing_calls`.
/// The bridge serialises ranges as 1-based; this function inverts that mapping
/// before forwarding the item to the LSP server.
fn mcp_item_to_lsp(item: serde_json::Value) -> Result<CallHierarchyItem> {
    let mcp: CallHierarchyItemResult = serde_json::from_value(item)
        .map_err(|e| Error::InvalidToolParams(format!("Invalid call hierarchy item: {e}")))?;

    let uri = mcp.uri.parse::<lsp_types::Uri>().map_err(|e| {
        Error::InvalidToolParams(format!("Invalid URI in call hierarchy item: {e}"))
    })?;

    let detail = mcp.detail;
    let data = mcp.data;

    // Round-trip via serde: `convert_call_hierarchy_item` stored the kind as a u32
    // by serialising `SymbolKind`; we reverse this to reconstruct the same value.
    let kind: lsp_types::SymbolKind = serde_json::from_value(serde_json::json!(mcp.kind))
        .unwrap_or(lsp_types::SymbolKind::FUNCTION);

    Ok(CallHierarchyItem {
        name: mcp.name,
        kind,
        tags: None,
        detail,
        uri,
        range: denormalize_range(&mcp.range),
        selection_range: denormalize_range(&mcp.selection_range),
        data,
    })
}

/// Convert a 1-based MCP range back to a 0-based LSP range.
///
/// Used when MCP clients pass back a `CallHierarchyItemResult` that was
/// previously returned by `prepare_call_hierarchy` (which stores 1-based coords).
const fn denormalize_range(range: &Range) -> lsp_types::Range {
    lsp_types::Range {
        start: lsp_types::Position {
            line: range.start.line.saturating_sub(1),
            character: range.start.character.saturating_sub(1),
        },
        end: lsp_types::Position {
            line: range.end.line.saturating_sub(1),
            character: range.end.character.saturating_sub(1),
        },
    }
}

fn workspace_edit_document_changes(edit: WorkspaceEdit) -> Result<Vec<DocumentChanges>> {
    let mut result = Vec::new();
    if let Some(changes) = edit.changes {
        result.extend(changes.into_iter().map(|(uri, edits)| {
            DocumentChanges {
                uri: uri.to_string(),
                version: None,
                edits: edits
                    .into_iter()
                    .map(|edit| TextEdit {
                        range: normalize_range(edit.range),
                        new_text: edit.new_text,
                    })
                    .collect(),
            }
        }));
    }
    if result.is_empty()
        && let Some(document_changes) = edit.document_changes
    {
        match document_changes {
            lsp_types::DocumentChanges::Edits(edits) => {
                result.extend(edits.into_iter().map(|edit| {
                    DocumentChanges {
                        uri: edit.text_document.uri.to_string(),
                        version: edit.text_document.version,
                        edits: edit
                            .edits
                            .into_iter()
                            .map(|edit| match edit {
                                lsp_types::OneOf::Left(edit) => TextEdit {
                                    range: normalize_range(edit.range),
                                    new_text: edit.new_text,
                                },
                                lsp_types::OneOf::Right(edit) => TextEdit {
                                    range: normalize_range(edit.text_edit.range),
                                    new_text: edit.text_edit.new_text,
                                },
                            })
                            .collect(),
                    }
                }));
            }
            lsp_types::DocumentChanges::Operations(_) => {
                return Err(Error::UnsupportedWorkspaceEdit(
                    "rename returned a resource operation".to_string(),
                ));
            }
        }
    }
    Ok(result)
}

const fn normalize_range(range: lsp_types::Range) -> Range {
    Range {
        start: Position2D {
            line: range.start.line + 1,
            character: range.start.character + 1,
        },
        end: Position2D {
            line: range.end.line + 1,
            character: range.end.character + 1,
        },
    }
}

/// Convert LSP document symbol to MCP symbol.
fn convert_document_symbol(symbol: DocumentSymbol) -> Symbol {
    Symbol {
        name: symbol.name,
        kind: format!("{:?}", symbol.kind),
        range: normalize_range(symbol.range),
        selection_range: normalize_range(symbol.selection_range),
        children: symbol
            .children
            .map(|children| children.into_iter().map(convert_document_symbol).collect()),
    }
}

/// Convert LSP call hierarchy item to MCP call hierarchy item.
fn convert_call_hierarchy_item(item: CallHierarchyItem) -> CallHierarchyItemResult {
    CallHierarchyItemResult {
        name: item.name,
        kind: serde_json::to_value(item.kind)
            .ok()
            .and_then(|v| v.as_u64())
            .and_then(|n| u32::try_from(n).ok())
            .unwrap_or(0),
        detail: item.detail,
        uri: item.uri.to_string(),
        range: normalize_range(item.range),
        selection_range: normalize_range(item.selection_range),
        data: item.data,
    }
}

/// Convert LSP code action to MCP code action.
pub fn convert_code_action_or_command(
    action: lsp_types::CodeActionOrCommand,
    action_id: Option<String>,
) -> CodeAction {
    match action {
        lsp_types::CodeActionOrCommand::CodeAction(action) => {
            convert_code_action(action, action_id)
        }
        lsp_types::CodeActionOrCommand::Command(cmd) => {
            let arguments = cmd.arguments.unwrap_or_default();
            CodeAction {
                action_id,
                title: cmd.title.clone(),
                kind: None,
                diagnostics: Vec::new(),
                edit: None,
                workspace_edit: None,
                command: Some(CommandDescription {
                    title: cmd.title,
                    command: cmd.command,
                    arguments,
                }),
                is_preferred: false,
                disabled: None,
                data: None,
            }
        }
    }
}

pub fn convert_code_action(action: lsp_types::CodeAction, action_id: Option<String>) -> CodeAction {
    let workspace_edit = action
        .edit
        .as_ref()
        .and_then(|edit| serde_json::to_value(edit).ok());
    let diagnostics = action.diagnostics.map_or_else(Vec::new, |diags| {
        let mut result = Vec::with_capacity(diags.len());
        for d in diags {
            result.push(Diagnostic {
                range: normalize_range(d.range),
                severity: match d.severity {
                    Some(lsp_types::DiagnosticSeverity::ERROR) => DiagnosticSeverity::Error,
                    Some(lsp_types::DiagnosticSeverity::WARNING) => DiagnosticSeverity::Warning,
                    Some(lsp_types::DiagnosticSeverity::INFORMATION) => {
                        DiagnosticSeverity::Information
                    }
                    Some(lsp_types::DiagnosticSeverity::HINT) => DiagnosticSeverity::Hint,
                    _ => DiagnosticSeverity::Information,
                },
                message: d.message,
                code: d.code.map(|c| match c {
                    lsp_types::NumberOrString::Number(n) => n.to_string(),
                    lsp_types::NumberOrString::String(s) => s,
                }),
            });
        }
        result
    });

    let edit = action.edit.map(|edit| {
        let changes = edit.changes.map_or_else(Vec::new, |changes_map| {
            let mut result = Vec::with_capacity(changes_map.len());
            for (uri, edits) in changes_map {
                let mut text_edits = Vec::with_capacity(edits.len());
                for e in edits {
                    text_edits.push(TextEdit {
                        range: normalize_range(e.range),
                        new_text: e.new_text,
                    });
                }
                result.push(DocumentChanges {
                    uri: uri.to_string(),
                    version: None,
                    edits: text_edits,
                });
            }
            result
        });
        WorkspaceEditDescription { changes }
    });

    let command = action.command.map(|cmd| {
        let arguments = cmd.arguments.unwrap_or_else(Vec::new);
        CommandDescription {
            title: cmd.title,
            command: cmd.command,
            arguments,
        }
    });

    CodeAction {
        action_id,
        title: action.title,
        kind: action.kind.map(|k| k.as_str().to_string()),
        diagnostics,
        edit,
        workspace_edit,
        command,
        is_preferred: action.is_preferred.unwrap_or(false),
        disabled: action.disabled.map(|disabled| disabled.reason),
        data: action.data,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::fs;

    use tempfile::TempDir;
    use url::Url;

    use super::*;

    #[test]
    fn test_translator_new() {
        let translator = Translator::new();
        assert_eq!(translator.workspace_roots.len(), 0);
        assert_eq!(translator.lsp_clients.len(), 0);
        assert_eq!(translator.lsp_servers.len(), 0);
    }

    #[test]
    fn test_set_workspace_roots() {
        let mut translator = Translator::new();
        let roots = vec![PathBuf::from("/test/root1"), PathBuf::from("/test/root2")];
        translator.set_workspace_roots(roots.clone());
        assert_eq!(translator.workspace_roots, roots);
    }

    #[test]
    fn linked_project_options_include_all_cargo_manifests() {
        let first = TempDir::new().unwrap();
        let second = TempDir::new().unwrap();
        fs::write(first.path().join("Cargo.toml"), "[workspace]\nmembers=[]\n").unwrap();
        fs::write(
            second.path().join("Cargo.toml"),
            "[workspace]\nmembers=[]\n",
        )
        .unwrap();
        let config = LspServerConfig::rust_analyzer();

        let options = linked_project_initialization_options(
            &config,
            &[first.path().to_path_buf(), second.path().to_path_buf()],
        )
        .unwrap();

        assert_eq!(
            options,
            Some(serde_json::json!({
                "linkedProjects": [
                    first.path().join("Cargo.toml"),
                    second.path().join("Cargo.toml")
                ]
            }))
        );
    }

    #[test]
    fn linked_project_options_merge_existing_settings_without_duplicates() {
        let first = TempDir::new().unwrap();
        let second = TempDir::new().unwrap();
        fs::write(first.path().join("Cargo.toml"), "[workspace]\nmembers=[]\n").unwrap();
        fs::write(
            second.path().join("Cargo.toml"),
            "[workspace]\nmembers=[]\n",
        )
        .unwrap();
        let mut config = LspServerConfig::rust_analyzer();
        config.initialization_options = Some(serde_json::json!({
            "cargo": {"buildScripts": {"enable": true}},
            "linkedProjects": [first.path().join("Cargo.toml")]
        }));

        let options = linked_project_initialization_options(
            &config,
            &[first.path().to_path_buf(), second.path().to_path_buf()],
        )
        .unwrap()
        .unwrap();
        assert_eq!(options["cargo"]["buildScripts"]["enable"], true);
        assert_eq!(options["linkedProjects"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn linked_project_options_fail_closed_for_invalid_inputs() {
        let root = TempDir::new().unwrap();
        let second = TempDir::new().unwrap();
        fs::write(root.path().join("Cargo.toml"), "[workspace]\nmembers=[]\n").unwrap();
        let config = LspServerConfig::rust_analyzer();
        let missing_manifest = linked_project_initialization_options(
            &config,
            &[root.path().to_path_buf(), second.path().to_path_buf()],
        );
        assert!(matches!(missing_manifest, Err(Error::InvalidConfig(_))));

        let mut invalid = LspServerConfig::rust_analyzer();
        invalid.initialization_options = Some(serde_json::json!({"linkedProjects": "nope"}));
        assert!(matches!(
            linked_project_initialization_options(
                &invalid,
                &[root.path().to_path_buf(), root.path().to_path_buf()]
            ),
            Err(Error::InvalidConfig(_))
        ));
    }

    #[test]
    fn server_workspace_roots_use_nested_manifest_for_container_workspace() {
        let workspace = TempDir::new().unwrap();
        let nested = workspace.path().join("pkgs/ai-integrations");
        fs::create_dir_all(&nested).unwrap();
        fs::write(nested.join("Cargo.toml"), "[package]\nname = \"fixture\"\n").unwrap();

        let translator = Translator::new();
        let roots = translator.server_workspace_roots(
            &LspServerConfig::rust_analyzer(),
            &[workspace.path().to_path_buf()],
        );

        assert_eq!(roots, vec![nested]);
    }

    #[tokio::test]
    async fn applies_open_document_content_without_an_active_server() {
        let root = TempDir::new().unwrap();
        let path = root.path().join("open.rs");
        let mut translator = Translator::new();
        translator
            .document_tracker_mut()
            .open(path.clone(), "dirty\n".to_string())
            .unwrap();

        translator
            .apply_open_document_content(&path, 1, "updated\n".to_string())
            .await
            .unwrap();

        let document = translator.document_tracker().get(&path).unwrap();
        assert_eq!(document.version, 2);
        assert_eq!(document.content, "updated\n");
    }

    #[tokio::test]
    async fn activation_restarts_when_requested_roots_shrink() {
        let first = TempDir::new().unwrap();
        let second = TempDir::new().unwrap();
        fs::write(first.path().join("Cargo.toml"), "[workspace]\nmembers=[]\n").unwrap();
        fs::write(
            second.path().join("Cargo.toml"),
            "[workspace]\nmembers=[]\n",
        )
        .unwrap();
        let mut config = LspServerConfig::rust_analyzer();
        config.command = "/definitely/missing/rust-analyzer".to_string();

        let mut translator = Translator::new();
        translator.set_workspace_roots(vec![
            first.path().to_path_buf(),
            second.path().to_path_buf(),
        ]);
        translator.set_lsp_configs(vec![config.clone()], None);
        translator.lsp_roots.insert(
            config.language_id.clone(),
            vec![first.path().to_path_buf(), second.path().to_path_buf()],
        );
        translator
            .lsp_clients
            .insert(config.language_id.clone(), LspClient::new(config));

        let result = translator
            .activate_project_with_roots(vec![first.path().to_path_buf()])
            .await;

        assert!(matches!(result, Err(Error::LspInitFailed { .. })));
        assert!(translator.lsp_clients.is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn handshaken_rust_server_does_not_wait_for_quiescence() {
        use std::os::unix::fs::PermissionsExt;

        let root = TempDir::new().unwrap();
        fs::write(
            root.path().join("Cargo.toml"),
            "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        )
        .unwrap();
        let server = root.path().join("handshake-only-lsp.py");
        fs::write(
            &server,
            r#"#!/usr/bin/env python3
import json
import sys

def read_message():
    headers = b""
    while b"\r\n\r\n" not in headers:
        chunk = sys.stdin.buffer.read(1)
        if not chunk:
            return None
        headers += chunk
    length = next(
        int(line.split(b":", 1)[1].strip())
        for line in headers.split(b"\r\n")
        if line.lower().startswith(b"content-length:")
    )
    body = sys.stdin.buffer.read(length)
    return json.loads(body)

def send_message(message):
    body = json.dumps(message, separators=(",", ":")).encode()
    sys.stdout.buffer.write(b"Content-Length: " + str(len(body)).encode() + b"\r\n\r\n" + body)
    sys.stdout.buffer.flush()

while True:
    message = read_message()
    if message is None:
        break
    if message.get("method") == "initialize":
        send_message({"jsonrpc": "2.0", "id": message["id"], "result": {"capabilities": {}}})
    elif message.get("method") == "shutdown":
        send_message({"jsonrpc": "2.0", "id": message["id"], "result": None})
        break
"#,
        )
        .unwrap();
        let mut permissions = fs::metadata(&server).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&server, permissions).unwrap();

        let mut config = LspServerConfig::rust_analyzer();
        config.command = server.display().to_string();
        config.timeout_seconds = 1;
        config.heuristics = None;
        let mut translator = Translator::new();
        translator.set_lsp_configs(vec![config.clone()], Some(3));

        let result = translator.activate_project(root.path().to_path_buf()).await;

        assert!(result.is_ok());
        assert_eq!(translator.lsp_servers.len(), 1);
        drop(translator);

        let file = root.path().join("src/lib.rs");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, "pub fn fixture() {}\n").unwrap();
        let mut lazy = Translator::new()
            .with_extensions(HashMap::from([("rs".to_string(), "rust".to_string())]));
        lazy.set_workspace_roots(vec![root.path().to_path_buf()]);
        lazy.set_lsp_configs(vec![config], Some(3));

        lazy.ensure_client_for_file(&file).await.unwrap();

        assert_eq!(lazy.lsp_servers.len(), 1);
    }

    #[test]
    fn test_register_server() {
        let translator = Translator::new();

        // Initial state: no servers registered
        assert_eq!(translator.lsp_servers.len(), 0);

        // The register_server method exists and is callable
        // Full integration testing with real LspServer is done in integration tests
        // This unit test verifies the method signature and basic functionality

        // Note: We can't easily construct an LspServer in a unit test without async
        // and a real LSP server process. The actual registration functionality is
        // tested in integration tests (see rust_analyzer_tests.rs).
        // This test verifies the data structure is properly initialized.
    }

    #[test]
    fn project_root_for_file_prefers_registered_workspace_root() {
        let workspace = TempDir::new().unwrap();
        let nested = workspace.path().join("crates/mcpls-core");
        fs::create_dir_all(&nested).unwrap();
        fs::write(
            workspace.path().join("Cargo.toml"),
            "[workspace]\nmembers=[]\n",
        )
        .unwrap();
        fs::write(nested.join("Cargo.toml"), "[package]\nname=\"nested\"\n").unwrap();

        let mut translator = Translator::new();
        translator.set_workspace_roots(vec![workspace.path().to_path_buf()]);
        let file = nested.join("src/lib.rs");

        assert_eq!(
            translator.project_root_for_file(&file, &LspServerConfig::rust_analyzer()),
            Some(workspace.path().to_path_buf())
        );
    }

    #[test]
    fn project_root_for_file_uses_nested_manifest_in_container_workspace() {
        let workspace = TempDir::new().unwrap();
        let nested = workspace.path().join("pkgs/ai-integrations");
        fs::create_dir_all(nested.join("src")).unwrap();
        fs::write(nested.join("Cargo.toml"), "[package]\nname=\"nested\"\n").unwrap();

        let mut translator = Translator::new();
        translator.set_workspace_roots(vec![workspace.path().to_path_buf()]);
        let file = nested.join("src/lib.rs");

        assert_eq!(
            translator.project_root_for_file(&file, &LspServerConfig::rust_analyzer()),
            Some(nested)
        );
    }

    #[test]
    fn project_root_for_file_skips_marker_free_container_workspace() {
        let workspace = TempDir::new().unwrap();
        let file = workspace.path().join("src/lib.rs");
        fs::create_dir_all(file.parent().unwrap()).unwrap();

        let mut translator = Translator::new();
        translator.set_workspace_roots(vec![workspace.path().to_path_buf()]);

        assert_eq!(
            translator.project_root_for_file(&file, &LspServerConfig::rust_analyzer()),
            None
        );
    }

    #[test]
    fn test_get_client_for_file_server_initializing_when_expected() {
        // A configured/applicable language whose LSP client has not registered
        // yet (large solution still loading via OmniSharp) must surface
        // ServerInitializing — "wait and retry" — not NoServerForLanguage.
        let mut translator = Translator::new();
        let path = PathBuf::from("/ws/Assets/Scripts/Player.cs");
        let lang = detect_language(&path, &translator.extension_map);

        let mut expected = HashSet::new();
        expected.insert(lang.clone());
        translator.set_expected_languages(expected);

        let err = translator.get_client_for_file(&path).unwrap_err();
        assert!(matches!(err, Error::ServerInitializing(ref l) if *l == lang));
    }

    #[test]
    fn test_get_client_for_file_no_server_when_not_expected() {
        // When the language is not in the expected set (no server configured for
        // it at all), the error stays NoServerForLanguage.
        let translator = Translator::new();
        let path = PathBuf::from("/ws/Assets/Scripts/Player.cs");
        let lang = detect_language(&path, &translator.extension_map);

        let err = translator.get_client_for_file(&path).unwrap_err();
        assert!(matches!(err, Error::NoServerForLanguage(ref l) if *l == lang));
    }

    #[test]
    fn test_clear_expected_languages_reverts_to_no_server() {
        // After initialization fails the expected set is cleared; subsequent
        // lookups must fall back to NoServerForLanguage rather than keep
        // implying the server is still on its way.
        let mut translator = Translator::new();
        let path = PathBuf::from("/ws/Assets/Scripts/Player.cs");
        let lang = detect_language(&path, &translator.extension_map);

        let mut expected = HashSet::new();
        expected.insert(lang);
        translator.set_expected_languages(expected);
        translator.clear_expected_languages();

        let err = translator.get_client_for_file(&path).unwrap_err();
        assert!(matches!(err, Error::NoServerForLanguage(_)));
    }

    #[test]
    fn wait_for_language_ready_reports_initializing_without_waiting() {
        let mut translator = Translator::new();
        translator.set_lsp_configs(vec![LspServerConfig::rust_analyzer()], None);
        translator.set_expected_languages(HashSet::from(["rust".to_string()]));

        assert!(matches!(
            translator.wait_for_language_ready("rust"),
            Err(Error::ServerInitializing(language)) if language == "rust"
        ));
    }

    #[test]
    fn test_diagnostic_request_params_omit_optional_null_fields() {
        let uri = "file:///test.ts".parse().unwrap();
        let params = diagnostic_request_params(TextDocumentIdentifier { uri });
        let value = serde_json::to_value(params).unwrap();

        assert_eq!(value["textDocument"]["uri"], "file:///test.ts");
        assert!(value.get("identifier").is_none());
        assert!(value.get("previousResultId").is_none());
    }

    #[test]
    fn test_validate_path_no_workspace_roots() {
        let translator = Translator::new();
        let temp_dir = TempDir::new().unwrap();
        let test_file = temp_dir.path().join("test.rs");
        fs::write(&test_file, "fn main() {}").unwrap();

        // With no workspace roots, any valid path should be accepted
        let result = translator.validate_path(&test_file);
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_path_within_workspace() {
        let mut translator = Translator::new();
        let temp_dir = TempDir::new().unwrap();
        let workspace_root = temp_dir.path().to_path_buf();
        translator.set_workspace_roots(vec![workspace_root]);

        let test_file = temp_dir.path().join("test.rs");
        fs::write(&test_file, "fn main() {}").unwrap();

        let result = translator.validate_path(&test_file);
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_path_outside_workspace() {
        let mut translator = Translator::new();
        let temp_dir1 = TempDir::new().unwrap();
        let temp_dir2 = TempDir::new().unwrap();

        // Set workspace root to temp_dir1
        translator.set_workspace_roots(vec![temp_dir1.path().to_path_buf()]);

        // Create file in temp_dir2 (outside workspace)
        let test_file = temp_dir2.path().join("test.rs");
        fs::write(&test_file, "fn main() {}").unwrap();

        let result = translator.validate_path(&test_file);
        assert!(matches!(result, Err(Error::PathOutsideWorkspace(_))));
    }

    #[test]
    fn test_normalize_range() {
        let lsp_range = lsp_types::Range {
            start: lsp_types::Position {
                line: 0,
                character: 0,
            },
            end: lsp_types::Position {
                line: 2,
                character: 5,
            },
        };

        let mcp_range = normalize_range(lsp_range);
        assert_eq!(mcp_range.start.line, 1);
        assert_eq!(mcp_range.start.character, 1);
        assert_eq!(mcp_range.end.line, 3);
        assert_eq!(mcp_range.end.character, 6);
    }

    #[test]
    fn test_extract_hover_contents_string() {
        let marked_string = lsp_types::MarkedString::String("Test hover".to_string());
        let contents = lsp_types::HoverContents::Scalar(marked_string);
        let result = extract_hover_contents(contents);
        assert_eq!(result, "Test hover");
    }

    #[test]
    fn test_extract_hover_contents_language_string() {
        let marked_string = lsp_types::MarkedString::LanguageString(lsp_types::LanguageString {
            language: "rust".to_string(),
            value: "fn main() {}".to_string(),
        });
        let contents = lsp_types::HoverContents::Scalar(marked_string);
        let result = extract_hover_contents(contents);
        assert_eq!(result, "```rust\nfn main() {}\n```");
    }

    #[test]
    fn test_extract_hover_contents_markup() {
        let markup = lsp_types::MarkupContent {
            kind: lsp_types::MarkupKind::Markdown,
            value: "# Documentation".to_string(),
        };
        let contents = lsp_types::HoverContents::Markup(markup);
        let result = extract_hover_contents(contents);
        assert_eq!(result, "# Documentation");
    }

    #[tokio::test]
    async fn test_handle_workspace_symbol_no_server() {
        let mut translator = Translator::new();
        let result = translator
            .handle_workspace_symbol("test".to_string(), None, 100)
            .await;
        assert!(matches!(result, Err(Error::NoServerConfigured)));
    }

    #[cfg(unix)]
    async fn workspace_symbol_test_server(
        root: &Path,
        language_id: &str,
        supports_workspace_symbols: bool,
        symbol_name: &str,
        request_log: &Path,
    ) -> LspServer {
        use std::os::unix::fs::PermissionsExt;

        let script = root.join(format!("{language_id}-workspace-symbol-lsp.py"));
        fs::write(
            &script,
            r#"#!/usr/bin/env python3
import json
from pathlib import Path
import sys

supports = sys.argv[1] == "true"
symbol_name = sys.argv[2]
request_log = Path(sys.argv[3])

def read_message():
    headers = b""
    while b"\r\n\r\n" not in headers:
        chunk = sys.stdin.buffer.read(1)
        if not chunk:
            return None
        headers += chunk
    length = next(
        int(line.split(b":", 1)[1].strip())
        for line in headers.split(b"\r\n")
        if line.lower().startswith(b"content-length:")
    )
    return json.loads(sys.stdin.buffer.read(length))

def send_message(message):
    body = json.dumps(message, separators=(",", ":")).encode()
    sys.stdout.buffer.write(
        b"Content-Length: " + str(len(body)).encode() + b"\r\n\r\n" + body
    )
    sys.stdout.buffer.flush()

while True:
    message = read_message()
    if message is None:
        break
    method = message.get("method")
    if method == "initialize":
        send_message({
            "jsonrpc": "2.0",
            "id": message["id"],
            "result": {"capabilities": {"workspaceSymbolProvider": supports}},
        })
    elif method == "workspace/symbol":
        request_log.write_text("requested")
        send_message({
            "jsonrpc": "2.0",
            "id": message["id"],
            "result": [{
                "name": symbol_name,
                "kind": 12,
                "location": {
                    "uri": "file:///tmp/test.rs",
                    "range": {
                        "start": {"line": 0, "character": 0},
                        "end": {"line": 0, "character": 1},
                    },
                },
            }],
        })
    elif method == "shutdown":
        send_message({"jsonrpc": "2.0", "id": message["id"], "result": None})
        break
"#,
        )
        .unwrap();
        let mut permissions = fs::metadata(&script).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script, permissions).unwrap();

        let config = LspServerConfig {
            language_id: language_id.to_string(),
            command: script.display().to_string(),
            args: vec![
                supports_workspace_symbols.to_string(),
                symbol_name.to_string(),
                request_log.display().to_string(),
            ],
            env: HashMap::new(),
            file_patterns: Vec::new(),
            initialization_options: None,
            timeout_seconds: 5,
            heuristics: None,
        };
        LspServer::spawn(ServerInitConfig {
            server_config: config,
            workspace_roots: vec![root.to_path_buf()],
            initialization_options: None,
            notification_tx: None,
        })
        .await
        .unwrap()
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn workspace_symbol_skips_server_without_capability() {
        let root = TempDir::new().unwrap();
        let request_log = root.path().join("unsupported.log");
        let server = workspace_symbol_test_server(
            root.path(),
            "unsupported",
            false,
            "ignored",
            &request_log,
        )
        .await;
        let client = server.client().clone();
        let mut translator = Translator::new();
        translator.register_client("unsupported".to_string(), client);
        translator.register_server("unsupported".to_string(), server);

        let result = translator
            .handle_workspace_symbol("test".to_string(), None, 100)
            .await
            .unwrap();

        assert!(result.symbols.is_empty());
        assert!(!request_log.exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn workspace_symbol_queries_capable_server() {
        let root = TempDir::new().unwrap();
        let request_log = root.path().join("capable.log");
        let server =
            workspace_symbol_test_server(root.path(), "capable", true, "found", &request_log).await;
        let client = server.client().clone();
        let mut translator = Translator::new();
        translator.register_client("capable".to_string(), client);
        translator.register_server("capable".to_string(), server);

        let result = translator
            .handle_workspace_symbol("test".to_string(), None, 100)
            .await
            .unwrap();

        assert_eq!(result.symbols.len(), 1);
        assert_eq!(result.symbols[0].name, "found");
        assert!(request_log.exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn workspace_symbol_queries_only_capable_servers() {
        let root = TempDir::new().unwrap();
        let unsupported_log = root.path().join("unsupported.log");
        let capable_log = root.path().join("capable.log");
        let unsupported = workspace_symbol_test_server(
            root.path(),
            "aaa-unsupported",
            false,
            "ignored",
            &unsupported_log,
        )
        .await;
        let capable =
            workspace_symbol_test_server(root.path(), "zzz-capable", true, "found", &capable_log)
                .await;
        let mut translator = Translator::new();
        translator.register_client("aaa-unsupported".to_string(), unsupported.client().clone());
        translator.register_server("aaa-unsupported".to_string(), unsupported);
        translator.register_client("zzz-capable".to_string(), capable.client().clone());
        translator.register_server("zzz-capable".to_string(), capable);

        let result = translator
            .handle_workspace_symbol("test".to_string(), None, 100)
            .await
            .unwrap();

        assert_eq!(result.symbols.len(), 1);
        assert_eq!(result.symbols[0].name, "found");
        assert!(!unsupported_log.exists());
        assert!(capable_log.exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn workspace_symbol_aggregates_in_language_order() {
        let root = TempDir::new().unwrap();
        let zzz = workspace_symbol_test_server(
            root.path(),
            "zzz-capable",
            true,
            "second",
            &root.path().join("zzz.log"),
        )
        .await;
        let aaa = workspace_symbol_test_server(
            root.path(),
            "aaa-capable",
            true,
            "first",
            &root.path().join("aaa.log"),
        )
        .await;
        let mut translator = Translator::new();
        translator.register_client("zzz-capable".to_string(), zzz.client().clone());
        translator.register_server("zzz-capable".to_string(), zzz);
        translator.register_client("aaa-capable".to_string(), aaa.client().clone());
        translator.register_server("aaa-capable".to_string(), aaa);

        let result = translator
            .handle_workspace_symbol("test".to_string(), None, 2)
            .await
            .unwrap();

        assert_eq!(
            result
                .symbols
                .iter()
                .map(|symbol| symbol.name.as_str())
                .collect::<Vec<_>>(),
            vec!["first", "second"]
        );
    }

    #[tokio::test]
    async fn test_handle_code_actions_invalid_kind() {
        let mut translator = Translator::new();
        let result = translator
            .handle_code_actions(
                "/tmp/test.rs".to_string(),
                1,
                1,
                1,
                10,
                Some("invalid_kind".to_string()),
            )
            .await;
        assert!(matches!(result, Err(Error::InvalidToolParams(_))));
    }

    #[tokio::test]
    async fn test_handle_code_actions_valid_kind_quickfix() {
        use tempfile::TempDir;

        let mut translator = Translator::new();
        let temp_dir = TempDir::new().unwrap();
        let test_file = temp_dir.path().join("test.rs");
        fs::write(&test_file, "fn main() {}").unwrap();

        let result = translator
            .handle_code_actions(
                test_file.to_str().unwrap().to_string(),
                1,
                1,
                1,
                10,
                Some("quickfix".to_string()),
            )
            .await;
        // Will fail due to no LSP server, but validates kind is accepted
        assert!(result.is_err());
        assert!(!matches!(result, Err(Error::InvalidToolParams(_))));
    }

    #[tokio::test]
    async fn test_handle_code_actions_valid_kind_refactor() {
        use tempfile::TempDir;

        let mut translator = Translator::new();
        let temp_dir = TempDir::new().unwrap();
        let test_file = temp_dir.path().join("test.rs");
        fs::write(&test_file, "fn main() {}").unwrap();

        let result = translator
            .handle_code_actions(
                test_file.to_str().unwrap().to_string(),
                1,
                1,
                1,
                10,
                Some("refactor".to_string()),
            )
            .await;
        assert!(result.is_err());
        assert!(!matches!(result, Err(Error::InvalidToolParams(_))));
    }

    #[tokio::test]
    async fn test_handle_code_actions_valid_kind_refactor_extract() {
        use tempfile::TempDir;

        let mut translator = Translator::new();
        let temp_dir = TempDir::new().unwrap();
        let test_file = temp_dir.path().join("test.rs");
        fs::write(&test_file, "fn main() {}").unwrap();

        let result = translator
            .handle_code_actions(
                test_file.to_str().unwrap().to_string(),
                1,
                1,
                1,
                10,
                Some("refactor.extract".to_string()),
            )
            .await;
        assert!(result.is_err());
        assert!(!matches!(result, Err(Error::InvalidToolParams(_))));
    }

    #[tokio::test]
    async fn test_handle_code_actions_valid_kind_source() {
        use tempfile::TempDir;

        let mut translator = Translator::new();
        let temp_dir = TempDir::new().unwrap();
        let test_file = temp_dir.path().join("test.rs");
        fs::write(&test_file, "fn main() {}").unwrap();

        let result = translator
            .handle_code_actions(
                test_file.to_str().unwrap().to_string(),
                1,
                1,
                1,
                10,
                Some("source.organizeImports".to_string()),
            )
            .await;
        assert!(result.is_err());
        assert!(!matches!(result, Err(Error::InvalidToolParams(_))));
    }

    #[tokio::test]
    async fn test_handle_code_actions_invalid_range_zero() {
        let mut translator = Translator::new();
        let result = translator
            .handle_code_actions("/tmp/test.rs".to_string(), 0, 1, 1, 10, None)
            .await;
        assert!(matches!(result, Err(Error::InvalidToolParams(_))));
    }

    #[tokio::test]
    async fn test_handle_code_actions_invalid_range_order() {
        let mut translator = Translator::new();
        let result = translator
            .handle_code_actions("/tmp/test.rs".to_string(), 10, 5, 5, 1, None)
            .await;
        assert!(matches!(result, Err(Error::InvalidToolParams(_))));
    }

    #[tokio::test]
    async fn test_handle_code_actions_empty_range() {
        use tempfile::TempDir;

        let mut translator = Translator::new();
        let temp_dir = TempDir::new().unwrap();
        let test_file = temp_dir.path().join("test.rs");
        fs::write(&test_file, "fn main() {}").unwrap();

        // Empty range (same position) should be valid
        let result = translator
            .handle_code_actions(test_file.to_str().unwrap().to_string(), 1, 5, 1, 5, None)
            .await;
        // Will fail due to no LSP server, but validates range is accepted
        assert!(result.is_err());
        assert!(!matches!(result, Err(Error::InvalidToolParams(_))));
    }

    #[test]
    fn test_convert_code_action_minimal() {
        let lsp_action = lsp_types::CodeAction {
            title: "Fix issue".to_string(),
            kind: None,
            diagnostics: None,
            edit: None,
            command: None,
            is_preferred: None,
            disabled: None,
            data: None,
        };

        let result = convert_code_action(lsp_action, None);
        assert_eq!(result.title, "Fix issue");
        assert!(result.kind.is_none());
        assert!(result.diagnostics.is_empty());
        assert!(result.edit.is_none());
        assert!(result.command.is_none());
        assert!(!result.is_preferred);
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn test_convert_code_action_with_diagnostics_all_severities() {
        let lsp_diagnostics = vec![
            lsp_types::Diagnostic {
                range: lsp_types::Range {
                    start: lsp_types::Position {
                        line: 0,
                        character: 0,
                    },
                    end: lsp_types::Position {
                        line: 0,
                        character: 5,
                    },
                },
                severity: Some(lsp_types::DiagnosticSeverity::ERROR),
                message: "Error message".to_string(),
                code: Some(lsp_types::NumberOrString::Number(1)),
                source: None,
                code_description: None,
                related_information: None,
                tags: None,
                data: None,
            },
            lsp_types::Diagnostic {
                range: lsp_types::Range {
                    start: lsp_types::Position {
                        line: 1,
                        character: 0,
                    },
                    end: lsp_types::Position {
                        line: 1,
                        character: 5,
                    },
                },
                severity: Some(lsp_types::DiagnosticSeverity::WARNING),
                message: "Warning message".to_string(),
                code: Some(lsp_types::NumberOrString::String("W001".to_string())),
                source: None,
                code_description: None,
                related_information: None,
                tags: None,
                data: None,
            },
            lsp_types::Diagnostic {
                range: lsp_types::Range {
                    start: lsp_types::Position {
                        line: 2,
                        character: 0,
                    },
                    end: lsp_types::Position {
                        line: 2,
                        character: 5,
                    },
                },
                severity: Some(lsp_types::DiagnosticSeverity::INFORMATION),
                message: "Info message".to_string(),
                code: None,
                source: None,
                code_description: None,
                related_information: None,
                tags: None,
                data: None,
            },
            lsp_types::Diagnostic {
                range: lsp_types::Range {
                    start: lsp_types::Position {
                        line: 3,
                        character: 0,
                    },
                    end: lsp_types::Position {
                        line: 3,
                        character: 5,
                    },
                },
                severity: Some(lsp_types::DiagnosticSeverity::HINT),
                message: "Hint message".to_string(),
                code: None,
                source: None,
                code_description: None,
                related_information: None,
                tags: None,
                data: None,
            },
        ];

        let lsp_action = lsp_types::CodeAction {
            title: "Fix all issues".to_string(),
            kind: Some(lsp_types::CodeActionKind::QUICKFIX),
            diagnostics: Some(lsp_diagnostics),
            edit: None,
            command: None,
            is_preferred: None,
            disabled: None,
            data: None,
        };

        let result = convert_code_action(lsp_action, None);
        assert_eq!(result.diagnostics.len(), 4);
        assert!(matches!(
            result.diagnostics[0].severity,
            DiagnosticSeverity::Error
        ));
        assert!(matches!(
            result.diagnostics[1].severity,
            DiagnosticSeverity::Warning
        ));
        assert!(matches!(
            result.diagnostics[2].severity,
            DiagnosticSeverity::Information
        ));
        assert!(matches!(
            result.diagnostics[3].severity,
            DiagnosticSeverity::Hint
        ));
        assert_eq!(result.diagnostics[0].code, Some("1".to_string()));
        assert_eq!(result.diagnostics[1].code, Some("W001".to_string()));
    }

    #[test]
    #[allow(clippy::mutable_key_type)]
    fn test_convert_code_action_with_workspace_edit() {
        use std::collections::HashMap;
        use std::str::FromStr;

        let uri = lsp_types::Uri::from_str("file:///test.rs").unwrap();
        let mut changes_map = HashMap::new();
        changes_map.insert(
            uri,
            vec![lsp_types::TextEdit {
                range: lsp_types::Range {
                    start: lsp_types::Position {
                        line: 0,
                        character: 0,
                    },
                    end: lsp_types::Position {
                        line: 0,
                        character: 5,
                    },
                },
                new_text: "fixed".to_string(),
            }],
        );

        let lsp_action = lsp_types::CodeAction {
            title: "Apply fix".to_string(),
            kind: Some(lsp_types::CodeActionKind::QUICKFIX),
            diagnostics: None,
            edit: Some(lsp_types::WorkspaceEdit {
                changes: Some(changes_map),
                document_changes: None,
                change_annotations: None,
            }),
            command: None,
            is_preferred: Some(true),
            disabled: None,
            data: None,
        };

        let result = convert_code_action(lsp_action, None);
        assert!(result.edit.is_some());
        let edit = result.edit.unwrap();
        assert_eq!(edit.changes.len(), 1);
        assert_eq!(edit.changes[0].uri, "file:///test.rs");
        assert_eq!(edit.changes[0].edits.len(), 1);
        assert_eq!(edit.changes[0].edits[0].new_text, "fixed");
        assert!(result.is_preferred);
    }

    #[test]
    fn test_convert_code_action_with_command() {
        let lsp_action = lsp_types::CodeAction {
            title: "Run command".to_string(),
            kind: Some(lsp_types::CodeActionKind::REFACTOR),
            diagnostics: None,
            edit: None,
            command: Some(lsp_types::Command {
                title: "Execute refactor".to_string(),
                command: "refactor.extract".to_string(),
                arguments: Some(vec![serde_json::json!("arg1"), serde_json::json!(42)]),
            }),
            is_preferred: None,
            disabled: None,
            data: None,
        };

        let result = convert_code_action(lsp_action, None);
        assert!(result.command.is_some());
        let cmd = result.command.unwrap();
        assert_eq!(cmd.title, "Execute refactor");
        assert_eq!(cmd.command, "refactor.extract");
        assert_eq!(cmd.arguments.len(), 2);
    }

    #[tokio::test]
    async fn test_handle_call_hierarchy_prepare_invalid_position_zero() {
        let mut translator = Translator::new();
        let result = translator
            .handle_call_hierarchy_prepare("/tmp/test.rs".to_string(), 0, 1)
            .await;
        assert!(matches!(result, Err(Error::InvalidToolParams(_))));

        let result = translator
            .handle_call_hierarchy_prepare("/tmp/test.rs".to_string(), 1, 0)
            .await;
        assert!(matches!(result, Err(Error::InvalidToolParams(_))));
    }

    #[tokio::test]
    async fn test_handle_call_hierarchy_prepare_invalid_position_too_large() {
        let mut translator = Translator::new();
        let result = translator
            .handle_call_hierarchy_prepare("/tmp/test.rs".to_string(), 1_000_001, 1)
            .await;
        assert!(matches!(result, Err(Error::InvalidToolParams(_))));

        let result = translator
            .handle_call_hierarchy_prepare("/tmp/test.rs".to_string(), 1, 1_000_001)
            .await;
        assert!(matches!(result, Err(Error::InvalidToolParams(_))));
    }

    #[tokio::test]
    async fn test_handle_incoming_calls_invalid_json() {
        let mut translator = Translator::new();
        let invalid_item = serde_json::json!({"invalid": "structure"});
        let result = translator.handle_incoming_calls(invalid_item).await;
        assert!(matches!(result, Err(Error::InvalidToolParams(_))));
    }

    #[tokio::test]
    async fn test_handle_outgoing_calls_invalid_json() {
        let mut translator = Translator::new();
        let invalid_item = serde_json::json!({"invalid": "structure"});
        let result = translator.handle_outgoing_calls(invalid_item).await;
        assert!(matches!(result, Err(Error::InvalidToolParams(_))));
    }

    #[tokio::test]
    async fn test_parse_file_uri_invalid_scheme() {
        let translator = Translator::new();
        let uri: lsp_types::Uri = "http://example.com/file.rs".parse().unwrap();
        let result = translator.parse_file_uri(&uri);
        assert!(matches!(result, Err(Error::InvalidToolParams(_))));
    }

    #[tokio::test]
    async fn test_parse_file_uri_valid_scheme() {
        let translator = Translator::new();
        let temp_dir = TempDir::new().unwrap();
        let test_file = temp_dir.path().join("test.rs");
        fs::write(&test_file, "fn main() {}").unwrap();

        // Use url crate for cross-platform file URI creation
        let file_url = Url::from_file_path(&test_file).unwrap();
        let uri: lsp_types::Uri = file_url.as_str().parse().unwrap();
        let result = translator.parse_file_uri(&uri);
        assert!(result.is_ok());
    }

    #[test]
    fn test_handle_cached_diagnostics_empty() {
        let mut translator = Translator::new();
        let temp_dir = TempDir::new().unwrap();
        let test_file = temp_dir.path().join("test.rs");
        fs::write(&test_file, "fn main() {}").unwrap();

        let result = translator.handle_cached_diagnostics(test_file.to_str().unwrap());
        assert!(result.is_ok());
        let diags = result.unwrap();
        assert_eq!(diags.diagnostics.len(), 0);
    }

    #[test]
    fn test_handle_server_logs_with_filter() {
        use crate::bridge::notifications::LogLevel;

        let mut translator = Translator::new();

        // Add some logs
        translator
            .notification_cache_mut()
            .store_log(LogLevel::Error, "error msg".to_string());
        translator
            .notification_cache_mut()
            .store_log(LogLevel::Warning, "warning msg".to_string());
        translator
            .notification_cache_mut()
            .store_log(LogLevel::Info, "info msg".to_string());
        translator
            .notification_cache_mut()
            .store_log(LogLevel::Debug, "debug msg".to_string());

        // Test with error filter
        let result = translator.handle_server_logs(10, Some("error".to_string()));
        assert!(result.is_ok());
        let logs = result.unwrap();
        assert_eq!(logs.logs.len(), 1);
        assert_eq!(logs.logs[0].message, "error msg");

        // Test with warning filter (includes error and warning)
        let result = translator.handle_server_logs(10, Some("warning".to_string()));
        assert!(result.is_ok());
        let logs = result.unwrap();
        assert_eq!(logs.logs.len(), 2);

        // Test with info filter (excludes debug)
        let result = translator.handle_server_logs(10, Some("info".to_string()));
        assert!(result.is_ok());
        let logs = result.unwrap();
        assert_eq!(logs.logs.len(), 3);

        // Test with debug filter (includes all)
        let result = translator.handle_server_logs(10, Some("debug".to_string()));
        assert!(result.is_ok());
        let logs = result.unwrap();
        assert_eq!(logs.logs.len(), 4);

        // Test with invalid filter
        let result = translator.handle_server_logs(10, Some("invalid".to_string()));
        assert!(matches!(result, Err(Error::InvalidToolParams(_))));
    }

    #[test]
    fn server_logs_redact_configured_environment_values() {
        use crate::bridge::notifications::LogLevel;

        let mut config = LspServerConfig::rust_analyzer();
        config
            .env
            .insert("MCPLS_TEST_SECRET".to_string(), "env-secret".to_string());

        let mut translator = Translator::new();
        translator.set_lsp_configs(vec![config], None);
        translator
            .notification_cache_mut()
            .store_log(LogLevel::Error, "server failed with env-secret".to_string());

        let result = translator.handle_server_logs(10, None).unwrap();
        assert_eq!(result.logs[0].message, "server failed with [REDACTED]");
    }

    #[test]
    fn server_notifications_redact_bearer_and_sensitive_assignments() {
        use crate::bridge::notifications::{LogLevel, MessageType};

        let mut translator = Translator::new();
        translator.notification_cache_mut().store_log(
            LogLevel::Error,
            "Bearer bearer-secret token=inline-secret".to_string(),
        );
        translator
            .notification_cache_mut()
            .store_message(MessageType::Error, "password: inline-password".to_string());

        let logs = translator.handle_server_logs(10, None).unwrap();
        assert!(!logs.logs[0].message.contains("bearer-secret"));
        assert!(!logs.logs[0].message.contains("inline-secret"));

        let messages = translator.handle_server_messages(10).unwrap();
        assert!(!messages.messages[0].message.contains("inline-password"));
    }

    #[test]
    fn project_redaction_patterns_flow_to_notifications() {
        let config = ProjectConfig {
            redaction_patterns: Some(vec!["configured-pattern".to_string()]),
            ..ProjectConfig::default()
        };
        let template = TranslatorTemplate::default().with_project_config(&config);
        let mut translator = template.translator_for_root(std::env::temp_dir());
        translator.notification_cache_mut().store_message(
            crate::bridge::notifications::MessageType::Info,
            "configured-pattern".to_string(),
        );

        let messages = translator.handle_server_messages(10).unwrap();
        assert_eq!(messages.messages[0].message, "[REDACTED]");
    }

    #[test]
    fn test_handle_server_messages_limit() {
        use crate::bridge::notifications::MessageType;

        let mut translator = Translator::new();

        // Add some messages
        for i in 0..10 {
            translator
                .notification_cache_mut()
                .store_message(MessageType::Info, format!("message {i}"));
        }

        // Test limit
        let result = translator.handle_server_messages(5);
        assert!(result.is_ok());
        let messages = result.unwrap();
        assert_eq!(messages.messages.len(), 5);
        assert_eq!(messages.messages[0].message, "message 0");
        assert_eq!(messages.messages[4].message, "message 4");

        // Test limit larger than available
        let result = translator.handle_server_messages(100);
        assert!(result.is_ok());
        let messages = result.unwrap();
        assert_eq!(messages.messages.len(), 10);
    }

    #[test]
    fn test_handle_cached_diagnostics_with_data() {
        let mut translator = Translator::new();
        let temp_dir = TempDir::new().unwrap();
        let test_file = temp_dir.path().join("test.rs");
        fs::write(&test_file, "fn main() {}").unwrap();

        let canonical_path = test_file.canonicalize().unwrap();
        let uri: lsp_types::Uri = Url::from_file_path(&canonical_path)
            .unwrap()
            .as_str()
            .parse()
            .unwrap();
        let diagnostic = lsp_types::Diagnostic {
            range: lsp_types::Range {
                start: lsp_types::Position {
                    line: 0,
                    character: 0,
                },
                end: lsp_types::Position {
                    line: 0,
                    character: 5,
                },
            },
            severity: Some(lsp_types::DiagnosticSeverity::ERROR),
            message: "test error".to_string(),
            code: Some(lsp_types::NumberOrString::String("E001".to_string())),
            source: None,
            code_description: None,
            related_information: None,
            tags: None,
            data: None,
        };

        translator
            .notification_cache_mut()
            .store_diagnostics(&uri, Some(1), vec![diagnostic]);

        let result = translator.handle_cached_diagnostics(test_file.to_str().unwrap());
        assert!(result.is_ok());
        let diags = result.unwrap();
        assert_eq!(diags.diagnostics.len(), 1);
        assert_eq!(diags.diagnostics[0].message, "test error");
        assert_eq!(diags.diagnostics[0].code, Some("E001".to_string()));
        assert!(matches!(
            diags.diagnostics[0].severity,
            DiagnosticSeverity::Error
        ));
        assert_eq!(diags.diagnostics[0].range.start.line, 1);
        assert_eq!(diags.diagnostics[0].range.start.character, 1);
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn test_handle_cached_diagnostics_multiple_severities() {
        let mut translator = Translator::new();
        let temp_dir = TempDir::new().unwrap();
        let test_file = temp_dir.path().join("test.rs");
        fs::write(&test_file, "fn main() {}").unwrap();

        let canonical_path = test_file.canonicalize().unwrap();
        let uri: lsp_types::Uri = Url::from_file_path(&canonical_path)
            .unwrap()
            .as_str()
            .parse()
            .unwrap();
        let diagnostics = vec![
            lsp_types::Diagnostic {
                range: lsp_types::Range {
                    start: lsp_types::Position {
                        line: 0,
                        character: 0,
                    },
                    end: lsp_types::Position {
                        line: 0,
                        character: 5,
                    },
                },
                severity: Some(lsp_types::DiagnosticSeverity::ERROR),
                message: "error".to_string(),
                code: None,
                source: None,
                code_description: None,
                related_information: None,
                tags: None,
                data: None,
            },
            lsp_types::Diagnostic {
                range: lsp_types::Range {
                    start: lsp_types::Position {
                        line: 1,
                        character: 0,
                    },
                    end: lsp_types::Position {
                        line: 1,
                        character: 5,
                    },
                },
                severity: Some(lsp_types::DiagnosticSeverity::WARNING),
                message: "warning".to_string(),
                code: None,
                source: None,
                code_description: None,
                related_information: None,
                tags: None,
                data: None,
            },
            lsp_types::Diagnostic {
                range: lsp_types::Range {
                    start: lsp_types::Position {
                        line: 2,
                        character: 0,
                    },
                    end: lsp_types::Position {
                        line: 2,
                        character: 5,
                    },
                },
                severity: Some(lsp_types::DiagnosticSeverity::INFORMATION),
                message: "info".to_string(),
                code: None,
                source: None,
                code_description: None,
                related_information: None,
                tags: None,
                data: None,
            },
            lsp_types::Diagnostic {
                range: lsp_types::Range {
                    start: lsp_types::Position {
                        line: 3,
                        character: 0,
                    },
                    end: lsp_types::Position {
                        line: 3,
                        character: 5,
                    },
                },
                severity: Some(lsp_types::DiagnosticSeverity::HINT),
                message: "hint".to_string(),
                code: None,
                source: None,
                code_description: None,
                related_information: None,
                tags: None,
                data: None,
            },
        ];

        translator
            .notification_cache_mut()
            .store_diagnostics(&uri, Some(1), diagnostics);

        let result = translator.handle_cached_diagnostics(test_file.to_str().unwrap());
        assert!(result.is_ok());
        let diags = result.unwrap();
        assert_eq!(diags.diagnostics.len(), 4);
        assert!(matches!(
            diags.diagnostics[0].severity,
            DiagnosticSeverity::Error
        ));
        assert!(matches!(
            diags.diagnostics[1].severity,
            DiagnosticSeverity::Warning
        ));
        assert!(matches!(
            diags.diagnostics[2].severity,
            DiagnosticSeverity::Information
        ));
        assert!(matches!(
            diags.diagnostics[3].severity,
            DiagnosticSeverity::Hint
        ));
    }

    #[test]
    fn test_handle_cached_diagnostics_with_numeric_code() {
        let mut translator = Translator::new();
        let temp_dir = TempDir::new().unwrap();
        let test_file = temp_dir.path().join("test.rs");
        fs::write(&test_file, "fn main() {}").unwrap();

        let canonical_path = test_file.canonicalize().unwrap();
        let uri: lsp_types::Uri = Url::from_file_path(&canonical_path)
            .unwrap()
            .as_str()
            .parse()
            .unwrap();
        let diagnostic = lsp_types::Diagnostic {
            range: lsp_types::Range {
                start: lsp_types::Position {
                    line: 0,
                    character: 0,
                },
                end: lsp_types::Position {
                    line: 0,
                    character: 5,
                },
            },
            severity: Some(lsp_types::DiagnosticSeverity::ERROR),
            message: "test error".to_string(),
            code: Some(lsp_types::NumberOrString::Number(42)),
            source: None,
            code_description: None,
            related_information: None,
            tags: None,
            data: None,
        };

        translator
            .notification_cache_mut()
            .store_diagnostics(&uri, Some(1), vec![diagnostic]);

        let result = translator.handle_cached_diagnostics(test_file.to_str().unwrap());
        assert!(result.is_ok());
        let diags = result.unwrap();
        assert_eq!(diags.diagnostics.len(), 1);
        assert_eq!(diags.diagnostics[0].code, Some("42".to_string()));
    }

    #[test]
    fn test_handle_cached_diagnostics_invalid_path() {
        let mut translator = Translator::new();
        let result = translator.handle_cached_diagnostics("/nonexistent/path/file.rs");
        assert!(matches!(result, Err(Error::FileIo { .. })));
    }

    #[test]
    fn test_handle_server_logs_no_filter() {
        use crate::bridge::notifications::LogLevel;

        let mut translator = Translator::new();

        translator
            .notification_cache_mut()
            .store_log(LogLevel::Error, "error msg".to_string());
        translator
            .notification_cache_mut()
            .store_log(LogLevel::Warning, "warning msg".to_string());
        translator
            .notification_cache_mut()
            .store_log(LogLevel::Info, "info msg".to_string());
        translator
            .notification_cache_mut()
            .store_log(LogLevel::Debug, "debug msg".to_string());

        let result = translator.handle_server_logs(10, None);
        assert!(result.is_ok());
        let logs = result.unwrap();
        assert_eq!(logs.logs.len(), 4);
    }

    #[test]
    fn test_handle_server_logs_error_filter_strict() {
        use crate::bridge::notifications::LogLevel;

        let mut translator = Translator::new();

        translator
            .notification_cache_mut()
            .store_log(LogLevel::Error, "error msg".to_string());
        translator
            .notification_cache_mut()
            .store_log(LogLevel::Warning, "warning msg".to_string());
        translator
            .notification_cache_mut()
            .store_log(LogLevel::Info, "info msg".to_string());

        let result = translator.handle_server_logs(10, Some("error".to_string()));
        assert!(result.is_ok());
        let logs = result.unwrap();
        assert_eq!(logs.logs.len(), 1);
        assert_eq!(logs.logs[0].message, "error msg");
    }

    #[test]
    fn test_handle_server_logs_warning_filter_includes_errors() {
        use crate::bridge::notifications::LogLevel;

        let mut translator = Translator::new();

        translator
            .notification_cache_mut()
            .store_log(LogLevel::Error, "error msg".to_string());
        translator
            .notification_cache_mut()
            .store_log(LogLevel::Warning, "warning msg".to_string());
        translator
            .notification_cache_mut()
            .store_log(LogLevel::Info, "info msg".to_string());

        let result = translator.handle_server_logs(10, Some("warning".to_string()));
        assert!(result.is_ok());
        let logs = result.unwrap();
        assert_eq!(logs.logs.len(), 2);
    }

    #[test]
    fn test_handle_server_logs_info_filter_excludes_debug() {
        use crate::bridge::notifications::LogLevel;

        let mut translator = Translator::new();

        translator
            .notification_cache_mut()
            .store_log(LogLevel::Error, "error msg".to_string());
        translator
            .notification_cache_mut()
            .store_log(LogLevel::Info, "info msg".to_string());
        translator
            .notification_cache_mut()
            .store_log(LogLevel::Debug, "debug msg".to_string());

        let result = translator.handle_server_logs(10, Some("info".to_string()));
        assert!(result.is_ok());
        let logs = result.unwrap();
        assert_eq!(logs.logs.len(), 2);
    }

    #[test]
    fn test_handle_server_logs_debug_filter_includes_all() {
        use crate::bridge::notifications::LogLevel;

        let mut translator = Translator::new();

        translator
            .notification_cache_mut()
            .store_log(LogLevel::Error, "error msg".to_string());
        translator
            .notification_cache_mut()
            .store_log(LogLevel::Warning, "warning msg".to_string());
        translator
            .notification_cache_mut()
            .store_log(LogLevel::Info, "info msg".to_string());
        translator
            .notification_cache_mut()
            .store_log(LogLevel::Debug, "debug msg".to_string());

        let result = translator.handle_server_logs(10, Some("debug".to_string()));
        assert!(result.is_ok());
        let logs = result.unwrap();
        assert_eq!(logs.logs.len(), 4);
    }

    #[test]
    fn test_handle_server_logs_limit_applies_after_filter() {
        use crate::bridge::notifications::LogLevel;

        let mut translator = Translator::new();

        for i in 0..10 {
            translator
                .notification_cache_mut()
                .store_log(LogLevel::Error, format!("error {i}"));
        }

        let result = translator.handle_server_logs(5, Some("error".to_string()));
        assert!(result.is_ok());
        let logs = result.unwrap();
        assert_eq!(logs.logs.len(), 5);
        assert_eq!(logs.logs[0].message, "error 0");
        assert_eq!(logs.logs[4].message, "error 4");
    }

    #[test]
    fn test_handle_server_logs_case_insensitive_level() {
        use crate::bridge::notifications::LogLevel;

        let mut translator = Translator::new();

        translator
            .notification_cache_mut()
            .store_log(LogLevel::Error, "error msg".to_string());

        let result = translator.handle_server_logs(10, Some("ERROR".to_string()));
        assert!(result.is_ok());

        let result = translator.handle_server_logs(10, Some("Error".to_string()));
        assert!(result.is_ok());

        let result = translator.handle_server_logs(10, Some("eRrOr".to_string()));
        assert!(result.is_ok());
    }

    #[test]
    fn test_handle_server_messages_empty() {
        let mut translator = Translator::new();

        let result = translator.handle_server_messages(10);
        assert!(result.is_ok());
        let messages = result.unwrap();
        assert_eq!(messages.messages.len(), 0);
    }

    #[test]
    fn test_handle_server_messages_with_different_types() {
        use crate::bridge::notifications::MessageType;

        let mut translator = Translator::new();

        translator
            .notification_cache_mut()
            .store_message(MessageType::Error, "error".to_string());
        translator
            .notification_cache_mut()
            .store_message(MessageType::Warning, "warning".to_string());
        translator
            .notification_cache_mut()
            .store_message(MessageType::Info, "info".to_string());
        translator
            .notification_cache_mut()
            .store_message(MessageType::Log, "log".to_string());

        let result = translator.handle_server_messages(10);
        assert!(result.is_ok());
        let messages = result.unwrap();
        assert_eq!(messages.messages.len(), 4);
        assert_eq!(messages.messages[0].message, "error");
        assert_eq!(messages.messages[1].message, "warning");
        assert_eq!(messages.messages[2].message, "info");
        assert_eq!(messages.messages[3].message, "log");
    }

    #[test]
    fn test_handle_server_messages_zero_limit() {
        use crate::bridge::notifications::MessageType;

        let mut translator = Translator::new();

        translator
            .notification_cache_mut()
            .store_message(MessageType::Info, "test".to_string());

        let result = translator.handle_server_messages(0);
        assert!(result.is_ok());
        let messages = result.unwrap();
        assert_eq!(messages.messages.len(), 0);
    }

    #[test]
    fn test_handle_cached_diagnostics_path_outside_workspace() {
        let mut translator = Translator::new();
        let temp_dir1 = TempDir::new().unwrap();
        let temp_dir2 = TempDir::new().unwrap();

        translator.set_workspace_roots(vec![temp_dir1.path().to_path_buf()]);

        let test_file = temp_dir2.path().join("test.rs");
        fs::write(&test_file, "fn main() {}").unwrap();

        let result = translator.handle_cached_diagnostics(test_file.to_str().unwrap());
        assert!(matches!(result, Err(Error::PathOutsideWorkspace(_))));
    }

    #[test]
    fn test_translator_with_custom_extensions() {
        let mut extension_map = HashMap::new();
        extension_map.insert("nu".to_string(), "nushell".to_string());
        extension_map.insert("customext".to_string(), "customlang".to_string());

        let translator = Translator::new().with_extensions(extension_map.clone());

        assert_eq!(translator.extension_map.len(), 2);
        assert_eq!(
            translator.extension_map.get("nu"),
            Some(&"nushell".to_string())
        );
        assert_eq!(
            translator.extension_map.get("customext"),
            Some(&"customlang".to_string())
        );
    }

    #[test]
    fn test_get_client_for_file_uses_custom_extension() {
        let temp_dir = TempDir::new().unwrap();
        let test_file = temp_dir.path().join("script.nu");
        fs::write(&test_file, "echo hello").unwrap();

        let mut extension_map = HashMap::new();
        extension_map.insert("nu".to_string(), "nushell".to_string());

        let translator = Translator::new().with_extensions(extension_map);

        let result = translator.get_client_for_file(&test_file);

        assert!(result.is_err());
        if let Err(Error::NoServerForLanguage(lang)) = result {
            assert_eq!(lang, "nushell");
        } else {
            panic!("Expected NoServerForLanguage(nushell) error");
        }
    }

    #[test]
    fn test_get_client_for_file_falls_back_to_default() {
        let temp_dir = TempDir::new().unwrap();
        let test_file = temp_dir.path().join("unknown.xyz");
        fs::write(&test_file, "content").unwrap();

        let mut extension_map = HashMap::new();
        extension_map.insert("rs".to_string(), "rust".to_string());

        let translator = Translator::new().with_extensions(extension_map);

        let result = translator.get_client_for_file(&test_file);

        assert!(result.is_err());
        if let Err(Error::NoServerForLanguage(lang)) = result {
            assert_eq!(lang, "plaintext");
        } else {
            panic!("Expected NoServerForLanguage(plaintext) error");
        }
    }

    #[tokio::test]
    async fn test_serve_initializes_translator_with_extensions() {
        use crate::config::{LanguageExtensionMapping, WorkspaceConfig};

        let language_extensions = vec![
            LanguageExtensionMapping {
                extensions: vec!["nu".to_string()],
                language_id: "nushell".to_string(),
            },
            LanguageExtensionMapping {
                extensions: vec!["rs".to_string()],
                language_id: "rust".to_string(),
            },
        ];

        let config = crate::config::ServerConfig {
            workspace: WorkspaceConfig {
                roots: vec![PathBuf::from("/tmp/test-workspace")],
                position_encodings: vec!["utf-8".to_string()],
                language_extensions: language_extensions.clone(),
                heuristics_max_depth: 10,
            },
            lsp_servers: vec![],
            daemon: crate::config::DaemonConfig::default(),
        };

        let extension_map = config.build_effective_extension_map();
        assert_eq!(extension_map.get("nu"), Some(&"nushell".to_string()));
        assert_eq!(extension_map.get("rs"), Some(&"rust".to_string()));

        // serve() starts in protocol-only mode when no LSP servers are configured;
        // it may return a transport error but must not return NoServersAvailable.
        let result = crate::serve(config).await;
        if let Err(ref err) = result {
            assert!(
                !matches!(err, crate::error::Error::NoServersAvailable(_)),
                "serve() must not return NoServersAvailable for empty lsp_servers config"
            );
        }
    }

    #[test]
    fn test_convert_call_hierarchy_item_kind_is_numeric() {
        let item = lsp_types::CallHierarchyItem {
            name: "my_fn".to_string(),
            kind: lsp_types::SymbolKind::FUNCTION,
            tags: None,
            detail: None,
            uri: "file:///tmp/test.rs".parse().unwrap(),
            range: lsp_types::Range {
                start: lsp_types::Position {
                    line: 0,
                    character: 0,
                },
                end: lsp_types::Position {
                    line: 0,
                    character: 5,
                },
            },
            selection_range: lsp_types::Range {
                start: lsp_types::Position {
                    line: 0,
                    character: 0,
                },
                end: lsp_types::Position {
                    line: 0,
                    character: 5,
                },
            },
            data: None,
        };
        let result = convert_call_hierarchy_item(item);
        // SymbolKind::FUNCTION is LSP integer 12
        assert_eq!(result.kind, 12u32);
        assert_eq!(result.name, "my_fn");
    }

    #[test]
    fn dead_lsp_shutdown_is_recoverable_during_restart() {
        assert!(shutdown_error_is_recoverable(&Error::ServerTerminated));
        assert!(!shutdown_error_is_recoverable(&Error::Timeout(30)));
    }
}
