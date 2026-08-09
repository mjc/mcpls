//! Project-actor lifecycle and configuration support.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use lsp_types::{
    DidChangeTextDocumentParams, TextDocumentContentChangeEvent, VersionedTextDocumentIdentifier,
};
use serde::Serialize;
use tokio::sync::mpsc;

use super::Translator;
use crate::bridge::notifications::RedactionPolicy;
use crate::bridge::{DocumentTracker, NotificationCache, lock_std, uri_to_path};
use crate::config::{
    LspServerConfig, ProjectConfig, ServerId, ToolRouter, default_position_encodings,
};
use crate::error::{Error, Result};
use crate::lsp::{
    LspNotification, LspServer, ServerInitConfig, apply_project_environment,
    load_project_environment,
};

/// Health of a project activation across its configured language servers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivationHealth {
    /// Every applicable language server initialized successfully.
    Ready,
    /// At least one applicable language server failed while another started.
    Degraded,
    /// No configured language server applies; structural tools remain usable.
    StructuralOnly,
}

/// Language-server handles produced by one project activation.
#[derive(Debug)]
pub struct ProjectActivation {
    notification_receivers: Vec<(ServerId, mpsc::Receiver<LspNotification>)>,
    health: ActivationHealth,
}

impl ProjectActivation {
    pub(crate) const fn ready() -> Self {
        Self::new(Vec::new(), ActivationHealth::Ready)
    }

    pub(crate) const fn structural_only() -> Self {
        Self::new(Vec::new(), ActivationHealth::StructuralOnly)
    }

    pub(crate) const fn new(
        notification_receivers: Vec<(ServerId, mpsc::Receiver<LspNotification>)>,
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

    /// Consume the activation and return each server's notification stream.
    #[must_use]
    pub fn into_notification_receivers(self) -> Vec<(ServerId, mpsc::Receiver<LspNotification>)> {
        self.notification_receivers
    }
}

/// Serializable description of one active server's negotiated capabilities.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerCapability {
    /// Configured language identifier for this server.
    pub language_id: String,
    /// Position encoding selected during initialization.
    pub position_encoding: String,
    /// Complete LSP capability object advertised by the server.
    pub capabilities: serde_json::Value,
}

/// Configuration snapshot used to construct an isolated project translator.
#[derive(Debug, Clone, Serialize, Default)]
pub struct TranslatorTemplate {
    extension_map: HashMap<String, String>,
    lsp_configs: Vec<LspServerConfig>,
    redaction_patterns: Vec<String>,
    heuristics_max_depth: Option<usize>,
    edit_safety: Option<crate::config::EditSafetyConfig>,
    position_encodings: Vec<String>,
}

impl TranslatorTemplate {
    #[must_use]
    pub(crate) fn language_applies_to_root(&self, language_id: &str, root: &Path) -> bool {
        self.lsp_configs
            .iter()
            .filter(|config| config.language_id.eq_ignore_ascii_case(language_id))
            .any(|config| {
                config.heuristics.as_ref().is_none_or(|heuristics| {
                    heuristics.is_applicable_recursive(root, self.heuristics_max_depth)
                })
            })
    }

    #[must_use]
    pub(crate) fn from_server_config(config: &crate::config::ServerConfig) -> Self {
        let mut template = Self::from_configuration(
            config.build_effective_extension_map(),
            config.lsp_servers.clone(),
            Some(config.workspace.heuristics_max_depth),
        );
        template.edit_safety.clone_from(&config.daemon.edit_safety);
        template
            .position_encodings
            .clone_from(&config.workspace.position_encodings);
        template
    }

    #[must_use]
    pub(crate) fn from_configuration(
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
            position_encodings: default_position_encodings(),
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

    /// Apply project-local server, heuristic, redaction, and edit-safety overrides.
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

    /// Build a fresh translator configured for one project root.
    #[must_use]
    pub fn translator_for_root(&self, root: PathBuf) -> Translator {
        let mut translator = Translator::new().with_extensions(self.extension_map.clone());
        translator.set_workspace_roots(vec![root]);
        translator.set_lsp_configs(self.lsp_configs.clone(), self.heuristics_max_depth);
        translator.set_redaction_patterns(self.redaction_patterns.clone());
        translator
            .position_encodings
            .clone_from(&self.position_encodings);
        translator
    }
}

impl Translator {
    /// Capture declarative configuration without live clients or documents.
    #[must_use]
    pub fn configuration_template(&self) -> TranslatorTemplate {
        TranslatorTemplate {
            extension_map: (*self.extension_map).clone(),
            lsp_configs: lock_std(&self.project_lsp_configs).clone(),
            redaction_patterns: Vec::new(),
            heuristics_max_depth: self.heuristics_max_depth,
            edit_safety: None,
            position_encodings: self.position_encodings.clone(),
        }
    }

    /// Return the workspace roots owned by this translator.
    #[must_use]
    pub fn workspace_roots(&self) -> &[PathBuf] {
        &self.workspace_roots
    }

    /// Return configured language IDs in deterministic order.
    #[must_use]
    pub fn configured_language_ids(&self) -> Vec<String> {
        let mut languages: Vec<_> = lock_std(&self.project_lsp_configs)
            .iter()
            .map(|config| config.language_id.clone())
            .collect();
        languages.sort();
        languages.dedup();
        languages
    }

    /// Return active language IDs in deterministic order.
    #[must_use]
    pub fn active_language_ids(&self) -> Vec<String> {
        let active = lock_std(&self.lsp_clients);
        let mut languages: Vec<_> = lock_std(&self.project_lsp_configs)
            .iter()
            .filter(|config| active.contains_key(&config.id()))
            .map(|config| config.language_id.clone())
            .collect();
        languages.sort();
        languages.dedup();
        languages
    }

    /// Return whether active servers already own exactly these roots.
    #[must_use]
    pub fn has_active_workspace_roots(&self, roots: &[PathBuf]) -> bool {
        let clients = lock_std(&self.lsp_clients);
        let registered_roots = lock_std(&self.project_lsp_roots);
        let configs = lock_std(&self.project_lsp_configs);
        !clients.is_empty()
            && self.has_workspace_roots(roots)
            && clients.keys().all(|id| {
                configs
                    .iter()
                    .find(|config| config.id() == *id)
                    .zip(registered_roots.get(id))
                    .is_some_and(|(config, registered)| {
                        same_workspace_roots(
                            registered,
                            &self.server_workspace_roots(config, roots),
                        )
                    })
            })
    }

    /// Return whether this translator owns exactly these logical project roots.
    #[must_use]
    pub(crate) fn has_workspace_roots(&self, roots: &[PathBuf]) -> bool {
        same_workspace_roots(&self.workspace_roots, roots)
    }

    /// Return negotiated capabilities for active language servers.
    ///
    /// # Errors
    ///
    /// Returns an error if negotiated capabilities cannot be serialized.
    pub fn server_capabilities(&self, language_id: Option<&str>) -> Result<Vec<ServerCapability>> {
        let configs = lock_std(&self.project_lsp_configs);
        let mut capabilities = lock_std(&self.lsp_servers)
            .iter()
            .filter_map(|(id, server)| {
                let config = configs.iter().find(|config| config.id() == *id)?;
                language_id
                    .is_none_or(|requested| config.language_id.eq_ignore_ascii_case(requested))
                    .then(|| {
                        Ok(ServerCapability {
                            language_id: config.language_id.clone(),
                            position_encoding: format!("{:?}", server.position_encoding()),
                            capabilities: serde_json::to_value(server.capabilities())?,
                        })
                    })
            })
            .collect::<Result<Vec<_>>>()?;
        capabilities.sort_by(|left, right| left.language_id.cmp(&right.language_id));
        Ok(capabilities)
    }

    /// Return the number of documents tracked by this project translator.
    #[must_use]
    pub fn open_document_count(&self) -> usize {
        self.document_tracker.len()
    }

    /// Mark language IDs as expected but not yet registered.
    pub fn set_expected_languages(&self, languages: HashSet<String>) {
        self.set_expected_servers(languages.into_iter().map(ServerId::from).collect());
    }

    /// Clear the expected-server set.
    pub fn clear_expected_languages(&self) {
        self.clear_expected_servers();
    }

    pub(crate) fn clear_expected_server(&self, server_id: &ServerId) {
        lock_std(&self.expected_servers).remove(server_id);
    }

    /// Return whether at least one configured server is still initializing.
    #[must_use]
    pub fn is_initializing(&self) -> bool {
        !lock_std(&self.expected_servers).is_empty()
    }

    /// Replace declarative LSP configurations and marker-search depth.
    pub fn set_lsp_configs(&mut self, configs: Vec<LspServerConfig>, max_depth: Option<usize>) {
        self.redaction_policy = RedactionPolicy::from_secrets(
            configs
                .iter()
                .flat_map(|config| config.env.values())
                .cloned(),
        );
        *lock_std(&self.project_lsp_configs) = configs;
        self.heuristics_max_depth = max_depth;
    }

    /// Replace values redacted from analyzer output.
    pub fn set_redaction_patterns(&mut self, patterns: Vec<String>) {
        let secrets = lock_std(&self.project_lsp_configs)
            .iter()
            .flat_map(|config| config.env.values())
            .cloned()
            .chain(patterns)
            .collect::<Vec<_>>();
        self.redaction_policy = RedactionPolicy::from_secrets(secrets);
    }

    /// Record the workspace roots owned by one active server.
    pub fn register_server_roots(&self, server_id: impl Into<ServerId>, roots: Vec<PathBuf>) {
        lock_std(&self.project_lsp_roots).insert(server_id.into(), roots);
    }

    /// Start every applicable server for one project root.
    ///
    /// # Errors
    ///
    /// Returns an error when every applicable server fails to start.
    pub async fn activate_project(&mut self, root: PathBuf) -> Result<ProjectActivation> {
        self.activate_project_with_roots(vec![root]).await
    }

    /// Start every applicable server for a logical project spanning `roots`.
    ///
    /// Reuses compatible servers and replaces servers whose effective roots
    /// changed. Tracked in-memory documents are reopened on replacements.
    ///
    /// # Errors
    ///
    /// Returns an error when every applicable server fails to start.
    #[allow(clippy::too_many_lines)]
    pub async fn activate_project_with_roots(
        &mut self,
        roots: Vec<PathBuf>,
    ) -> Result<ProjectActivation> {
        if roots.is_empty() {
            return Err(Error::NoServerConfigured);
        }
        let configs = lock_std(&self.project_lsp_configs)
            .iter()
            .filter(|config| {
                roots
                    .iter()
                    .any(|root| config.should_spawn(root, self.heuristics_max_depth))
            })
            .cloned()
            .collect::<Vec<_>>();
        let router = ToolRouter::from_configs(configs.iter())?;
        *lock_std(&self.router) = router;

        let configured_ids = configs
            .iter()
            .map(LspServerConfig::id)
            .collect::<HashSet<_>>();
        let stale_ids = lock_std(&self.lsp_servers)
            .keys()
            .filter(|id| !configured_ids.contains(*id))
            .cloned()
            .collect::<Vec<_>>();
        for id in stale_ids {
            let server = { lock_std(&self.lsp_servers).remove(&id) };
            if let Some(server) = server {
                let _ = server.shutdown().await;
            }
            lock_std(&self.lsp_clients).remove(&id);
            lock_std(&self.project_lsp_roots).remove(&id);
            lock_std(&self.server_configs).remove(&id);
            self.document_tracker.forget_server(&id);
        }
        if configs.is_empty() {
            self.clear_expected_servers();
            self.actor_notification_cache.set_diagnostics_route_count(0);
            self.set_workspace_roots(roots);
            return Ok(ProjectActivation::structural_only());
        }

        let pending = configs
            .iter()
            .filter(|config| !self.can_reuse_server(config, &roots))
            .cloned()
            .collect::<Vec<_>>();
        if pending.is_empty() {
            self.set_workspace_roots(roots);
            return Ok(ProjectActivation::ready());
        }

        let replaced = pending
            .iter()
            .filter_map(|config| lock_std(&self.lsp_servers).remove(&config.id()))
            .collect::<Vec<_>>();
        for server in replaced {
            let _ = server.shutdown().await;
        }
        for config in &pending {
            let id = config.id();
            lock_std(&self.lsp_clients).remove(&id);
            lock_std(&self.project_lsp_roots).remove(&id);
        }

        self.set_expected_servers(
            pending
                .iter()
                .filter(|config| config.language_id.eq_ignore_ascii_case("rust"))
                .map(LspServerConfig::id)
                .collect(),
        );
        let mut project_environments = HashMap::new();
        let mut init_configs = Vec::with_capacity(pending.len());
        for config in &pending {
            let server_roots = self.server_workspace_roots(config, &roots);
            let mut server_config = config.clone();
            if let Some(root) = server_roots.first() {
                if !project_environments.contains_key(root) {
                    project_environments.insert(root.clone(), load_project_environment(root).await);
                }
                if let Some(Some(project_environment)) = project_environments.get(root) {
                    apply_project_environment(&mut server_config, project_environment);
                }
            }
            init_configs.push(ServerInitConfig {
                initialization_options: rust_analyzer_initialization_options(
                    &server_config,
                    &server_roots,
                )?,
                server_config,
                workspace_roots: server_roots,
                position_encodings: self.position_encodings.clone(),
                notification_tx: None,
            });
        }
        self.set_workspace_roots(roots);
        let result = LspServer::spawn_batch(&init_configs).await;
        if result.all_failed() {
            self.clear_expected_servers();
            return Err(Error::LspInitFailed {
                message: result
                    .failures
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", "),
            });
        }

        let mut health = if result.failures.is_empty() {
            ActivationHealth::Ready
        } else {
            ActivationHealth::Degraded
        };
        let successful: HashSet<_> = result.servers.keys().cloned().collect();
        lock_std(&self.expected_servers).retain(|id| successful.contains(id));
        self.rebind_router(&successful);
        let init_by_id: HashMap<_, _> = init_configs
            .into_iter()
            .map(|config| (config.server_config.id(), config))
            .collect();
        let mut receivers = Vec::new();
        for (id, mut server) in result.servers {
            let client = server.client().clone();
            let server_roots = server.workspace_roots().to_vec();
            receivers.push((id.clone(), server.take_notification_rx()));
            self.register_server_roots(id.clone(), server_roots);
            self.register_client(id.clone(), client.clone());
            self.register_server(id.clone(), server);
            if let Some(config) = init_by_id.get(&id) {
                self.register_server_config(id.clone(), config.clone());
            }
            self.document_tracker.forget_server(&id);
            let language = init_by_id
                .get(&id)
                .map(|config| config.server_config.language_id.as_str());
            for document in self.document_tracker.open_documents() {
                if language
                    .is_some_and(|language| document.language_id().eq_ignore_ascii_case(language))
                    && let Some(path) = uri_to_path(document.uri())
                    && let Err(error) = self
                        .document_tracker
                        .sync_tracked(&path, &id, &client)
                        .await
                {
                    tracing::warn!(server_id = %id, path = %path.display(), %error,
                        "failed to reopen tracked document after language-server activation");
                    health = ActivationHealth::Degraded;
                }
            }
        }
        let diagnostics_routes = init_by_id
            .iter()
            .filter(|(id, config)| {
                successful.contains(*id)
                    && self.is_diagnostics_route(&config.server_config.language_id, id)
            })
            .count();
        self.actor_notification_cache
            .set_diagnostics_route_count(diagnostics_routes);
        Ok(ProjectActivation::new(receivers, health))
    }

    /// Stop all project-owned language servers and clear their routing state.
    ///
    /// # Errors
    ///
    /// Returns an error if server shutdown fails.
    pub async fn shutdown(&mut self) -> Result<()> {
        self.shutdown_servers().await;
        lock_std(&self.lsp_clients).clear();
        lock_std(&self.project_lsp_roots).clear();
        self.clear_expected_servers();
        Ok(())
    }

    /// Add a root and reactivate servers when their effective roots change.
    ///
    /// # Errors
    ///
    /// Returns an error when reactivation fails.
    pub async fn add_workspace_root(&mut self, root: PathBuf) -> Result<ProjectActivation> {
        if self.workspace_roots.contains(&root) {
            return Ok(ProjectActivation::ready());
        }
        let mut roots = (*self.workspace_roots).clone();
        roots.push(root);
        if lock_std(&self.lsp_clients).is_empty() || lock_std(&self.project_lsp_configs).is_empty()
        {
            self.set_workspace_roots(roots);
            return Ok(ProjectActivation::ready());
        }
        self.shutdown().await?;
        self.activate_project_with_roots(roots).await
    }

    /// Return this translator's interior-mutable document tracker.
    pub fn document_tracker_mut(&mut self) -> &DocumentTracker {
        &self.document_tracker
    }

    /// Pull diagnostics and merge them with this actor's push cache.
    ///
    /// # Errors
    ///
    /// Returns an error when the file cannot be routed or diagnostics fail.
    pub async fn handle_actor_diagnostics(
        &mut self,
        file_path: String,
    ) -> Result<super::DiagnosticsResult> {
        let cache = tokio::sync::Mutex::new(std::mem::take(&mut self.actor_notification_cache));
        let result = self.handle_diagnostics(file_path, &cache).await;
        self.actor_notification_cache = cache.into_inner();
        result
    }

    /// Read actor-owned push diagnostics for a workspace file.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid paths or cached diagnostic data.
    pub fn handle_cached_diagnostics(&self, file_path: &str) -> Result<super::DiagnosticsResult> {
        use super::{Diagnostic, DiagnosticSeverity, DiagnosticsResult, Position2D, Range};

        let uri = Self::cached_diagnostics_uri(&self.workspace_roots, file_path)?;
        let diagnostics = self
            .actor_notification_cache
            .get_diagnostics(&uri)
            .map_or_else(Vec::new, |entry| {
                entry
                    .diagnostics
                    .iter()
                    .map(|diagnostic| Diagnostic {
                        range: Range {
                            start: Position2D {
                                line: diagnostic.range.start.line + 1,
                                character: diagnostic.range.start.character + 1,
                            },
                            end: Position2D {
                                line: diagnostic.range.end.line + 1,
                                character: diagnostic.range.end.character + 1,
                            },
                        },
                        severity: match diagnostic.severity {
                            Some(lsp_types::DiagnosticSeverity::ERROR) => DiagnosticSeverity::Error,
                            Some(lsp_types::DiagnosticSeverity::WARNING) => {
                                DiagnosticSeverity::Warning
                            }
                            Some(lsp_types::DiagnosticSeverity::HINT) => DiagnosticSeverity::Hint,
                            _ => DiagnosticSeverity::Information,
                        },
                        message: diagnostic.message.clone(),
                        code: diagnostic.code.as_ref().map(|code| match code {
                            lsp_types::NumberOrString::Number(value) => value.to_string(),
                            lsp_types::NumberOrString::String(value) => value.clone(),
                        }),
                    })
                    .collect()
            });
        Ok(DiagnosticsResult { diagnostics })
    }

    /// Return whether actor-owned diagnostics exist for a workspace file.
    ///
    /// # Errors
    ///
    /// Returns an error when `file_path` is outside the project.
    pub fn has_cached_diagnostics(&self, file_path: &str) -> Result<bool> {
        let uri = Self::cached_diagnostics_uri(&self.workspace_roots, file_path)?;
        Ok(self
            .actor_notification_cache
            .get_diagnostics(&uri)
            .is_some())
    }

    /// Return bounded, redacted server log entries owned by this actor.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid level filter.
    pub fn actor_server_logs(
        &self,
        limit: usize,
        min_level: Option<String>,
    ) -> Result<super::ServerLogsResult> {
        let mut result =
            Self::handle_server_logs(&self.actor_notification_cache, limit, min_level)?;
        for log in &mut result.logs {
            log.message = self.redaction_policy.redact(&log.message);
        }
        Ok(result)
    }

    /// Return bounded, redacted user-facing server messages.
    ///
    /// # Errors
    ///
    /// Returns an error when the requested limit is invalid.
    pub fn actor_server_messages(&self, limit: usize) -> Result<super::ServerMessagesResult> {
        let mut result = Self::handle_server_messages(&self.actor_notification_cache, limit)?;
        for message in &mut result.messages {
            message.message = self.redaction_policy.redact(&message.message);
        }
        Ok(result)
    }

    /// Commit planned content to the tracker and notify each routed server.
    ///
    /// Notification failures are returned per server after the authoritative
    /// tracker state is committed, allowing apply results to report degraded
    /// synchronization without falsely reporting that filesystem commit failed.
    ///
    /// # Errors
    ///
    /// Returns an error if the document disappeared or its version changed.
    pub async fn apply_open_document_content(
        &self,
        path: &Path,
        expected_version: i32,
        content: String,
    ) -> Result<Vec<(ServerId, String)>> {
        let document = self
            .document_tracker
            .get(path)
            .ok_or_else(|| Error::DocumentNotFound(path.to_path_buf()))?;
        if document.version() != expected_version {
            return Err(Error::InvalidToolParams(format!(
                "document version changed for {}: expected {}, got {}",
                path.display(),
                expected_version,
                document.version()
            )));
        }
        let language_id = document.language_id().to_string();
        let next_version = self
            .document_tracker
            .update(path, content.clone())
            .ok_or_else(|| Error::DocumentNotFound(path.to_path_buf()))?;
        let mut failures = Vec::new();
        let clients = self.clients_for_language(&language_id);
        for (id, client) in clients {
            if let Err(error) = client
                .notify(
                    "textDocument/didChange",
                    DidChangeTextDocumentParams {
                        text_document: VersionedTextDocumentIdentifier {
                            uri: document.uri().clone(),
                            version: next_version,
                        },
                        content_changes: vec![TextDocumentContentChangeEvent {
                            range: None,
                            range_length: None,
                            text: content.clone(),
                        }],
                    },
                )
                .await
            {
                failures.push((id, error.to_string()));
            } else {
                self.document_tracker
                    .mark_server_synced(path, id, next_version);
            }
        }
        Ok(failures)
    }

    /// Return actor-owned notification state.
    #[must_use]
    pub const fn notification_cache(&self) -> &NotificationCache {
        &self.actor_notification_cache
    }

    /// Return mutable actor-owned notification state.
    pub const fn notification_cache_mut(&mut self) -> &mut NotificationCache {
        &mut self.actor_notification_cache
    }

    pub(super) fn clients_for_language(
        &self,
        language_id: &str,
    ) -> Vec<(ServerId, crate::lsp::LspClient)> {
        let configs = lock_std(&self.project_lsp_configs);
        let clients = lock_std(&self.lsp_clients);
        configs
            .iter()
            .filter(|config| config.language_id.eq_ignore_ascii_case(language_id))
            .filter_map(|config| {
                let id = config.id();
                clients.get(&id).cloned().map(|client| (id, client))
            })
            .collect()
    }

    fn can_reuse_server(&self, config: &LspServerConfig, roots: &[PathBuf]) -> bool {
        let id = config.id();
        lock_std(&self.lsp_clients).contains_key(&id)
            && lock_std(&self.project_lsp_roots)
                .get(&id)
                .is_some_and(|existing| {
                    same_workspace_roots(existing, &self.server_workspace_roots(config, roots))
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
}

fn same_workspace_roots(existing: &[PathBuf], requested: &[PathBuf]) -> bool {
    existing.len() == requested.len()
        && requested
            .iter()
            .all(|root| existing.iter().any(|existing| existing == root))
}

const RUST_ANALYZER_EXCLUDED_DIRECTORIES: &[&str] = &[
    ".direnv",
    ".git",
    ".next",
    ".nuxt",
    ".tox",
    ".venv",
    "__pycache__",
    "build",
    "coverage",
    "dist",
    "node_modules",
    "out",
    "target",
];

fn rust_analyzer_initialization_options(
    config: &LspServerConfig,
    roots: &[PathBuf],
) -> Result<Option<serde_json::Value>> {
    if !config.language_id.eq_ignore_ascii_case("rust") {
        return Ok(config.initialization_options.clone());
    }
    let mut options = match config.initialization_options.clone() {
        None => serde_json::Map::new(),
        Some(serde_json::Value::Object(options)) => options,
        Some(_) => {
            return Err(Error::InvalidConfig(
                "rust-analyzer initialization_options must be a JSON object".to_string(),
            ));
        }
    };
    merge_rust_analyzer_file_exclusions(&mut options, roots)?;
    set_default_rust_analyzer_symbol_search(&mut options)?;
    if roots.len() >= 2 {
        let mut linked = match options.remove("linkedProjects") {
            None => Vec::new(),
            Some(serde_json::Value::Array(projects)) => projects,
            Some(_) => {
                return Err(Error::InvalidConfig(
                    "rust-analyzer linkedProjects must be a JSON array".to_string(),
                ));
            }
        };
        for root in roots {
            let manifest = root.join("Cargo.toml");
            if !manifest.is_file() {
                return Err(Error::InvalidConfig(format!(
                    "shared rust project has no Cargo.toml: {}",
                    root.display()
                )));
            }
            let value = serde_json::Value::String(manifest.to_string_lossy().into_owned());
            if !linked.contains(&value) {
                linked.push(value);
            }
        }
        options.insert(
            "linkedProjects".to_string(),
            serde_json::Value::Array(linked),
        );
    }
    Ok(Some(serde_json::Value::Object(options)))
}

#[cfg(feature = "bench")]
pub fn benchmark_rust_analyzer_initialization_options(
    roots: &[PathBuf],
) -> Result<serde_json::Value> {
    rust_analyzer_initialization_options(&LspServerConfig::rust_analyzer(), roots)?
        .ok_or_else(|| Error::InvalidConfig("rust benchmark options were absent".to_string()))
}

fn set_default_rust_analyzer_symbol_search(
    options: &mut serde_json::Map<String, serde_json::Value>,
) -> Result<()> {
    let mut current = options;
    for key in ["workspace", "symbol", "search"] {
        current = current
            .entry(key.to_string())
            .or_insert_with(|| serde_json::json!({}))
            .as_object_mut()
            .ok_or_else(|| {
                Error::InvalidConfig(format!(
                    "rust-analyzer initialization_options.{key} must be a JSON object"
                ))
            })?;
    }
    current
        .entry("kind".to_string())
        .or_insert_with(|| serde_json::json!("all_symbols"));
    Ok(())
}

fn merge_rust_analyzer_file_exclusions(
    options: &mut serde_json::Map<String, serde_json::Value>,
    roots: &[PathBuf],
) -> Result<()> {
    let excludes = options
        .entry("files".to_string())
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .ok_or_else(|| {
            Error::InvalidConfig(
                "rust-analyzer initialization_options.files must be a JSON object".to_string(),
            )
        })?
        .entry("exclude".to_string())
        .or_insert_with(|| serde_json::json!([]))
        .as_array_mut()
        .ok_or_else(|| {
            Error::InvalidConfig(
                "rust-analyzer initialization_options.files.exclude must be a JSON array"
                    .to_string(),
            )
        })?;
    for path in rust_analyzer_excluded_paths(roots) {
        let value = serde_json::Value::String(path);
        if !excludes.contains(&value) {
            excludes.push(value);
        }
    }
    Ok(())
}

fn rust_analyzer_excluded_paths(roots: &[PathBuf]) -> Vec<String> {
    let mut excluded = Vec::new();
    for root in roots {
        let mut pending = vec![root.clone()];
        while let Some(directory) = pending.pop() {
            let Ok(entries) = std::fs::read_dir(&directory) else {
                continue;
            };
            for entry in entries.flatten() {
                let Ok(file_type) = entry.file_type() else {
                    continue;
                };
                if !file_type.is_dir() {
                    continue;
                }
                let path = entry.path();
                if RUST_ANALYZER_EXCLUDED_DIRECTORIES
                    .contains(&entry.file_name().to_string_lossy().as_ref())
                {
                    if let Ok(relative) = path.strip_prefix(root) {
                        let relative = relative.to_string_lossy().into_owned();
                        if !excluded.contains(&relative) {
                            excluded.push(relative);
                        }
                    }
                } else {
                    pending.push(path);
                }
            }
        }
    }
    excluded.sort();
    excluded
}
