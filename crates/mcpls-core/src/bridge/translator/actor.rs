//! Project-actor lifecycle and configuration support.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use lsp_types::{
    DidChangeTextDocumentParams, TextDocumentContentChangeEvent, VersionedTextDocumentIdentifier,
};
use serde::Serialize;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::{ActiveLanguageAlias, Translator};
use crate::bridge::notifications::RedactionPolicy;
use crate::bridge::{
    DocumentTracker, EncodingConverter, NotificationCache, PositionEncoding, lock_std, uri_to_path,
};
use crate::config::{
    CargoFeatureProfile, LspServerConfig, ProjectConfig, ServerId, ToolRouter,
    default_position_encodings,
};
use crate::error::{Error, Result};
use crate::lsp::{
    LspNotification, LspServer, ServerInitConfig, apply_project_environment,
    load_project_environment, resolve_command,
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
            for override_config in lsp_servers {
                if let Some(existing) = self.lsp_configs.iter_mut().find(|existing| {
                    existing.id() == override_config.id()
                        || existing.language_id == override_config.language_id
                }) {
                    existing.clone_from(override_config);
                } else {
                    self.lsp_configs.push(override_config.clone());
                }
            }
        }
        if let Some(profile) = &config.cargo_features {
            self.apply_cargo_feature_profile(profile);
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

    fn apply_cargo_feature_profile(&mut self, profile: &CargoFeatureProfile) {
        let Some(server) = self
            .lsp_configs
            .iter_mut()
            .find(|config| config.language_id.eq_ignore_ascii_case("rust"))
        else {
            return;
        };
        let mut options = server
            .initialization_options
            .take()
            .and_then(|value| value.as_object().cloned())
            .unwrap_or_default();
        let mut cargo = options
            .remove("cargo")
            .and_then(|value| value.as_object().cloned())
            .unwrap_or_default();
        let profile = profile.normalized();
        cargo.insert("features".to_string(), serde_json::json!(profile.features));
        cargo.insert(
            "allFeatures".to_string(),
            serde_json::json!(profile.all_features),
        );
        cargo.insert(
            "noDefaultFeatures".to_string(),
            serde_json::json!(profile.no_default_features),
        );
        options.insert("cargo".to_string(), serde_json::Value::Object(cargo));
        server.initialization_options = Some(serde_json::Value::Object(options));
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

#[cfg(test)]
fn apply_builtin_precedence(configs: &[LspServerConfig]) -> Vec<LspServerConfig> {
    configs
        .iter()
        .filter(|config| {
            !configs.iter().any(|candidate| {
                candidate.language_id != config.language_id
                    && candidate.builtin_profile().is_some_and(|profile| {
                        profile
                            .supersedes
                            .iter()
                            .any(|language| language.eq_ignore_ascii_case(&config.language_id))
                            && profile.file_patterns.iter().any(|pattern| {
                                config
                                    .file_patterns
                                    .iter()
                                    .any(|configured| configured == pattern)
                            })
                    })
            })
        })
        .cloned()
        .collect()
}

#[derive(Debug, Clone)]
struct PlannedServer {
    config: LspServerConfig,
    workspace_roots: Vec<PathBuf>,
}

fn planned_server_roots(servers: &[PlannedServer]) -> HashMap<ServerId, Vec<PathBuf>> {
    servers
        .iter()
        .map(|server| (server.config.id(), server.workspace_roots.clone()))
        .collect()
}

fn active_builtin_aliases<'a>(
    configs: impl IntoIterator<Item = &'a LspServerConfig>,
    successful: &HashSet<ServerId>,
    roots_by_id: &HashMap<ServerId, Vec<PathBuf>>,
) -> Vec<ActiveLanguageAlias> {
    let configs = configs.into_iter().collect::<Vec<_>>();
    let mut aliases = Vec::new();
    for specialist in configs.iter().copied() {
        if !successful.contains(&specialist.id()) {
            continue;
        }
        let Some(profile) = specialist.builtin_profile() else {
            continue;
        };
        for generic in configs.iter().copied().filter(|generic| {
            profile
                .supersedes
                .iter()
                .any(|language| language.eq_ignore_ascii_case(&generic.language_id))
                && profile.file_patterns.iter().any(|pattern| {
                    generic
                        .file_patterns
                        .iter()
                        .any(|configured| configured == pattern)
                })
        }) {
            for root in roots_by_id.get(&specialist.id()).into_iter().flatten() {
                aliases.push(ActiveLanguageAlias {
                    root: root.clone(),
                    language: generic.language_id.clone(),
                    specialist: specialist.language_id.clone(),
                });
            }
        }
    }
    aliases
}

fn builtin_command_available(
    config: &LspServerConfig,
    project_environment: Option<&HashMap<String, Option<String>>>,
) -> bool {
    !config.is_optional_builtin_profile()
        || resolve_command(&config.command, project_environment).is_file()
}

fn incremental_content_change(
    original: &str,
    updated: &str,
    encoding: PositionEncoding,
) -> TextDocumentContentChangeEvent {
    let mut prefix = original
        .bytes()
        .zip(updated.bytes())
        .take_while(|(left, right)| left == right)
        .count();
    while prefix > 0 && (!original.is_char_boundary(prefix) || !updated.is_char_boundary(prefix)) {
        prefix -= 1;
    }

    let max_suffix = original.len().min(updated.len()).saturating_sub(prefix);
    let mut suffix = original
        .bytes()
        .rev()
        .zip(updated.bytes().rev())
        .take(max_suffix)
        .take_while(|(left, right)| left == right)
        .count();
    while suffix > 0
        && (!original.is_char_boundary(original.len() - suffix)
            || !updated.is_char_boundary(updated.len() - suffix))
    {
        suffix -= 1;
    }

    let start = lsp_position_at(original, prefix, encoding);
    let end = lsp_position_at(original, original.len() - suffix, encoding);
    let range = start
        .zip(end)
        .map(|(start, end)| lsp_types::Range { start, end });
    TextDocumentContentChangeEvent {
        range,
        range_length: None,
        text: if range.is_some() {
            updated[prefix..updated.len() - suffix].to_string()
        } else {
            updated.to_string()
        },
    }
}

fn lsp_position_at(
    text: &str,
    byte_offset: usize,
    encoding: PositionEncoding,
) -> Option<lsp_types::Position> {
    let before = text.get(..byte_offset)?;
    let line = u32::try_from(before.bytes().filter(|byte| *byte == b'\n').count()).ok()?;
    let line_start = before.rfind('\n').map_or(0, |offset| offset + 1);
    let character = EncodingConverter::new(encoding)
        .byte_offset_to_character(&text[line_start..byte_offset], byte_offset - line_start)
        .ok()?;
    Some(lsp_types::Position::new(line, character))
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
        if !self.has_workspace_roots(roots) || lock_std(&self.lsp_clients).is_empty() {
            return false;
        }
        let configs = lock_std(&self.project_lsp_configs).clone();
        let current = self.applicable_servers(&configs, roots);
        *lock_std(&self.evaluated_lsp_roots) == planned_server_roots(&current)
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
        let server_id = server_id.into();
        lock_std(&self.evaluated_lsp_roots).insert(server_id.clone(), roots.clone());
        lock_std(&self.project_lsp_roots).insert(server_id, roots);
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
        self.activate_project_with_roots_cancelled(roots, CancellationToken::new())
            .await
    }

    /// Start every applicable server while honoring a caller-owned
    /// cancellation signal during process initialization.
    ///
    /// # Errors
    ///
    /// Returns an error when every applicable language server fails to start
    /// or initialization is cancelled.
    #[allow(clippy::too_many_lines)]
    pub async fn activate_project_with_roots_cancelled(
        &mut self,
        roots: Vec<PathBuf>,
        cancellation: CancellationToken,
    ) -> Result<ProjectActivation> {
        if roots.is_empty() {
            return Err(Error::NoServerConfigured);
        }
        lock_std(&self.active_language_aliases).clear();
        let all_configs = lock_std(&self.project_lsp_configs).clone();
        let servers = self.applicable_servers(&all_configs, &roots);
        drop(all_configs);
        *lock_std(&self.evaluated_lsp_roots) = planned_server_roots(&servers);
        let router = ToolRouter::from_configs(servers.iter().map(|server| &server.config))?;
        *lock_std(&self.router) = router;

        let configured_ids = servers
            .iter()
            .map(|server| server.config.id())
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
        if servers.is_empty() {
            self.clear_expected_servers();
            self.actor_notification_cache.set_diagnostics_route_count(0);
            self.set_workspace_roots(roots);
            return Ok(ProjectActivation::structural_only());
        }

        let pending = servers
            .iter()
            .filter(|server| !self.can_reuse_server(server))
            .collect::<Vec<_>>();
        if pending.is_empty() {
            let active_ids = lock_std(&self.lsp_servers).keys().cloned().collect();
            let active_roots = lock_std(&self.project_lsp_roots).clone();
            lock_std(&self.active_language_aliases).clone_from(&active_builtin_aliases(
                servers.iter().map(|server| &server.config),
                &active_ids,
                &active_roots,
            ));
            self.clear_expected_servers();
            self.set_workspace_roots(roots);
            return Ok(ProjectActivation::ready());
        }

        let replaced = pending
            .iter()
            .filter_map(|server| lock_std(&self.lsp_servers).remove(&server.config.id()))
            .collect::<Vec<_>>();
        for server in replaced {
            let _ = server.shutdown().await;
        }
        for server in &pending {
            let id = server.config.id();
            lock_std(&self.lsp_clients).remove(&id);
            lock_std(&self.project_lsp_roots).remove(&id);
        }

        self.set_expected_servers(
            pending
                .iter()
                .filter(|server| server.config.language_id.eq_ignore_ascii_case("rust"))
                .map(|server| server.config.id())
                .collect(),
        );
        let mut project_environments = HashMap::new();
        let mut init_configs = Vec::with_capacity(pending.len());
        for server in pending {
            let server_roots = server.workspace_roots.clone();
            let mut server_config = server.config.clone();
            if let Some(root) = server_roots.first() {
                if !project_environments.contains_key(root) {
                    project_environments.insert(root.clone(), load_project_environment(root).await);
                }
                if let Some(Some(project_environment)) = project_environments.get(root) {
                    apply_project_environment(&mut server_config, project_environment);
                }
            }
            let project_environment = server_roots
                .first()
                .and_then(|root| project_environments.get(root))
                .and_then(Option::as_ref);
            if !builtin_command_available(&server_config, project_environment) {
                tracing::debug!(
                    language = %server_config.language_id,
                    command = %server_config.command,
                    "skipping unavailable optional built-in language server"
                );
                self.clear_expected_server(&server_config.id());
                continue;
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
        if init_configs.is_empty() {
            self.clear_expected_servers();
            return Ok(if lock_std(&self.lsp_servers).is_empty() {
                ProjectActivation::structural_only()
            } else {
                ProjectActivation::ready()
            });
        }
        let result = LspServer::spawn_batch_with_cancellation(&init_configs, cancellation).await;
        if result.all_failed() {
            self.clear_expected_servers();
            lock_std(&self.active_language_aliases).clear();
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
        let active_roots = lock_std(&self.project_lsp_roots).clone();
        lock_std(&self.active_language_aliases).clone_from(&active_builtin_aliases(
            servers.iter().map(|server| &server.config),
            &successful,
            &active_roots,
        ));
        let diagnostics_routes = init_by_id
            .iter()
            .filter(|(id, config)| {
                successful.contains(*id)
                    && self.is_diagnostics_route(&config.server_config.language_id, id)
            })
            .count();
        self.actor_notification_cache
            .set_diagnostics_route_count(diagnostics_routes);
        self.clear_expected_servers();
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
        lock_std(&self.evaluated_lsp_roots).clear();
        lock_std(&self.active_language_aliases).clear();
        self.clear_expected_servers();
        Ok(())
    }

    /// Add a root and reactivate servers when their effective roots change.
    ///
    /// # Errors
    ///
    /// Returns an error when reactivation fails.
    pub async fn add_workspace_root(&mut self, root: PathBuf) -> Result<ProjectActivation> {
        self.add_workspace_root_cancelled(root, CancellationToken::new())
            .await
    }

    /// Add a root while honoring a caller-owned cancellation signal during
    /// any required server replacement.
    ///
    /// # Errors
    ///
    /// Returns an error when replacement activation fails.
    pub async fn add_workspace_root_cancelled(
        &mut self,
        root: PathBuf,
        cancellation: CancellationToken,
    ) -> Result<ProjectActivation> {
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
        self.activate_project_with_roots_cancelled(roots, cancellation)
            .await
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
        options: super::DiagnosticOptions,
    ) -> Result<super::DiagnosticsResult> {
        let cache = tokio::sync::Mutex::new(std::mem::take(&mut self.actor_notification_cache));
        let result = self
            .handle_diagnostics_with_options(file_path, &cache, options)
            .await;
        self.actor_notification_cache = cache.into_inner();
        result
    }

    /// Read actor-owned push diagnostics for a workspace file.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid paths or cached diagnostic data.
    pub async fn handle_cached_diagnostics(
        &self,
        file_path: &str,
        options: super::DiagnosticOptions,
    ) -> Result<super::DiagnosticsResult> {
        let uri = Self::cached_diagnostics_uri(&self.workspace_roots, file_path)?;
        let entry = self.actor_notification_cache.get_diagnostics(&uri);
        let cache = entry.map(|entry| super::DiagnosticsCacheMetadata {
            hit: true,
            age_ms: (chrono::Utc::now() - entry.received_at)
                .num_milliseconds()
                .max(0) as u64,
            document_version: entry.version,
        });
        let encoding = self
            .actor_notification_cache
            .diagnostics_owner(&uri)
            .map_or(crate::bridge::encoding::PositionEncoding::Utf16, |owner| {
                self.encoding_ctx(owner).encoding
            });
        let mut budget = super::source_context::SourceBudget::new(options.byte_limit);
        let result = Self::diagnostics_from_cache_entry_enriched(
            entry,
            encoding,
            &self.document_tracker,
            &self.workspace_roots,
            &self.redaction_policy,
            &mut budget,
        )
        .await;
        let mut result = Self::finish_diagnostics(result.diagnostics, options);
        result.cache = cache;
        Ok(result)
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
        let original_content = document.content().to_string();
        let next_version = self
            .document_tracker
            .update(path, content.clone())
            .ok_or_else(|| Error::DocumentNotFound(path.to_path_buf()))?;
        let mut failures = Vec::new();
        let clients = self.clients_for_language(&language_id);
        for (id, client) in clients {
            let content_change = incremental_content_change(
                &original_content,
                &content,
                self.position_encoding_for(&id),
            );
            if let Err(error) = client
                .notify(
                    "textDocument/didChange",
                    DidChangeTextDocumentParams {
                        text_document: VersionedTextDocumentIdentifier {
                            uri: document.uri().clone(),
                            version: next_version,
                        },
                        content_changes: vec![content_change],
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

    fn can_reuse_server(&self, server: &PlannedServer) -> bool {
        let id = server.config.id();
        lock_std(&self.lsp_clients).contains_key(&id)
            && lock_std(&self.project_lsp_roots)
                .get(&id)
                .is_some_and(|existing| same_workspace_roots(existing, &server.workspace_roots))
    }

    fn applicable_servers(
        &self,
        configs: &[LspServerConfig],
        roots: &[PathBuf],
    ) -> Vec<PlannedServer> {
        configs
            .iter()
            .filter_map(|config| {
                let workspace_roots = self.server_workspace_roots(config, roots);
                (!workspace_roots.is_empty()).then(|| PlannedServer {
                    config: config.clone(),
                    workspace_roots,
                })
            })
            .collect()
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn open_document_change_is_incremental_and_encoding_safe() {
        let original = "zero\nalpha 😀 omega\nlast\n";
        let updated = "zero\nalpha 😀 inserted omega\nlast\n";

        let change = incremental_content_change(original, updated, PositionEncoding::Utf16);

        assert_eq!(change.text, "inserted ");
        assert_eq!(
            change.range,
            Some(lsp_types::Range {
                start: lsp_types::Position::new(1, 9),
                end: lsp_types::Position::new(1, 9),
            })
        );
    }

    #[test]
    fn builtin_specialist_profiles_supersede_generic_profiles() {
        let defaults = crate::config::ServerConfig::default().lsp_servers;
        let typescript = defaults
            .iter()
            .find(|config| config.language_id == "typescript")
            .cloned()
            .unwrap();
        let vue = defaults
            .iter()
            .find(|config| config.language_id == "vue")
            .cloned()
            .unwrap();

        let filtered = apply_builtin_precedence(&[typescript.clone(), vue]);

        assert_eq!(filtered.len(), 2);

        let angular = defaults
            .iter()
            .find(|config| config.language_id == "angular")
            .cloned()
            .unwrap();
        let filtered = apply_builtin_precedence(&[typescript, angular]);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].language_id, "angular");

        let yaml = defaults
            .iter()
            .find(|config| config.language_id == "yaml")
            .cloned()
            .unwrap();
        let ansible = defaults
            .iter()
            .find(|config| config.language_id == "ansible")
            .cloned()
            .unwrap();
        let filtered = apply_builtin_precedence(&[yaml, ansible]);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].language_id, "ansible");
    }

    #[test]
    fn live_specialist_aliases_preserve_generic_fallbacks() {
        let defaults = crate::config::ServerConfig::default().lsp_servers;
        let angular = defaults
            .iter()
            .find(|config| config.language_id == "angular")
            .unwrap();
        let typescript = defaults
            .iter()
            .find(|config| config.language_id == "typescript")
            .unwrap();
        let ansible = defaults
            .iter()
            .find(|config| config.language_id == "ansible")
            .unwrap();
        let yaml = defaults
            .iter()
            .find(|config| config.language_id == "yaml")
            .unwrap();
        let configs = vec![
            typescript.clone(),
            angular.clone(),
            yaml.clone(),
            ansible.clone(),
        ];
        let successful = HashSet::from([angular.id(), ansible.id()]);
        let root = PathBuf::from("/workspace/frontend");
        let roots = HashMap::from([
            (angular.id(), vec![root.clone()]),
            (ansible.id(), vec![PathBuf::from("/workspace/automation")]),
        ]);

        let aliases = active_builtin_aliases(&configs, &successful, &roots);

        assert!(aliases.iter().any(|alias| {
            alias.root == root && alias.language == "typescript" && alias.specialist == "angular"
        }));
        assert!(
            aliases
                .iter()
                .any(|alias| { alias.language == "yaml" && alias.specialist == "ansible" })
        );
    }

    #[test]
    fn project_lsp_override_replaces_one_builtin_and_keeps_the_catalog() {
        let mut translator = Translator::new();
        translator.set_lsp_configs(crate::config::builtin_server_configs(), Some(10));
        let mut rust = LspServerConfig::rust_analyzer();
        rust.args.push("--log-file=mcpls-test.log".to_string());

        let merged = translator
            .configuration_template()
            .with_project_config(&ProjectConfig {
                lsp_servers: Some(vec![rust]),
                ..ProjectConfig::default()
            });

        assert!(
            merged
                .lsp_configs
                .iter()
                .any(|config| config.language_id == "python")
        );
        assert_eq!(
            merged
                .lsp_configs
                .iter()
                .find(|config| config.language_id == "rust")
                .unwrap()
                .args,
            vec!["--log-file=mcpls-test.log"]
        );
    }

    #[test]
    fn project_cargo_features_merge_into_rust_analyzer_initialization() {
        let mut translator = Translator::new();
        translator.set_lsp_configs(vec![LspServerConfig::rust_analyzer()], Some(10));

        let merged = translator
            .configuration_template()
            .with_project_config(&ProjectConfig {
                cargo_features: Some(CargoFeatureProfile {
                    features: vec!["zeta".to_string(), "alpha".to_string(), "alpha".to_string()],
                    all_features: false,
                    no_default_features: true,
                }),
                ..ProjectConfig::default()
            });
        let options = merged
            .rust_server_config()
            .unwrap()
            .initialization_options
            .as_ref()
            .unwrap();

        assert_eq!(
            options["cargo"]["features"],
            serde_json::json!(["alpha", "zeta"])
        );
        assert!(!options["cargo"]["allFeatures"].as_bool().unwrap());
        assert!(options["cargo"]["noDefaultFeatures"].as_bool().unwrap());
    }

    #[test]
    fn server_plan_uses_nested_marker_and_excludes_irrelevant_config() {
        let outer = TempDir::new().expect("temporary workspace");
        let nested = outer.path().join("pkgs/ai-integrations");
        std::fs::create_dir_all(&nested).expect("nested project directory");
        std::fs::write(nested.join("Cargo.toml"), "[workspace]\n").expect("nested Cargo manifest");

        let rust = LspServerConfig::rust_analyzer();
        let go = LspServerConfig::gopls();
        let mut translator = Translator::new();
        translator.set_lsp_configs(vec![rust.clone(), go.clone()], Some(10));
        let requested = vec![outer.path().to_path_buf()];
        let servers = translator.applicable_servers(&[rust.clone(), go], &requested);
        assert!(
            translator
                .configuration_template()
                .language_applies_to_root("rust", outer.path())
        );

        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].config.id(), rust.id());
        assert_eq!(servers[0].workspace_roots, vec![nested]);
    }

    #[tokio::test]
    async fn newly_applicable_server_invalidates_active_root_reuse() {
        use crate::bridge::translator::testing::fake_lsp_client;

        let root = TempDir::new().expect("temporary workspace");
        std::fs::write(root.path().join("Cargo.toml"), "[workspace]\n")
            .expect("Rust workspace marker");

        let rust = LspServerConfig::rust_analyzer();
        let go = LspServerConfig::gopls();
        let mut translator = Translator::new();
        translator.set_lsp_configs(vec![rust.clone(), go.clone()], Some(10));
        let roots = vec![root.path().to_path_buf()];
        translator.set_workspace_roots(roots.clone());
        let (client, _server) = fake_lsp_client();
        translator.register_client(rust.id(), client);
        translator.register_server_roots(rust.id(), roots.clone());

        assert!(translator.has_active_workspace_roots(&roots));

        std::fs::write(root.path().join("go.mod"), "module example.test\n")
            .expect("Go workspace marker");
        assert_eq!(translator.server_workspace_roots(&go, &roots), roots);

        assert!(!translator.has_active_workspace_roots(&roots));
    }
    #[tokio::test]
    async fn reused_activation_clears_initializing_state() {
        use crate::bridge::translator::testing::fake_lsp_client;

        let root = TempDir::new().expect("temporary workspace");
        std::fs::write(root.path().join("Cargo.toml"), "[workspace]\n")
            .expect("Rust workspace marker");

        let rust = LspServerConfig::rust_analyzer();
        let mut translator = Translator::new();
        translator.set_lsp_configs(vec![rust.clone()], Some(10));
        let roots = vec![root.path().to_path_buf()];
        translator.set_workspace_roots(roots.clone());
        let (client, _server) = fake_lsp_client();
        translator.register_client(rust.id(), client);
        translator.register_server_roots(rust.id(), roots.clone());
        translator.set_expected_servers(HashSet::from([rust.id()]));

        translator
            .activate_project_with_roots_cancelled(
                roots,
                tokio_util::sync::CancellationToken::new(),
            )
            .await
            .expect("reused server activation");

        assert!(!translator.is_initializing());
    }

    #[tokio::test]
    async fn unavailable_optional_builtin_keeps_project_structural_only() {
        let root = TempDir::new().expect("temporary project");
        std::fs::write(
            root.path().join("pyproject.toml"),
            "[project]\nname=\"fixture\"\n",
        )
        .expect("python marker");
        std::fs::write(root.path().join("main.py"), "print('fallback')\n").expect("python source");

        let mut config = LspServerConfig::pyright();
        config.command = "/definitely-missing/pyright-langserver".to_string();
        assert!(config.is_optional_builtin_profile());

        let mut translator = Translator::new();
        translator.set_lsp_configs(vec![config], Some(10));
        let activation = translator
            .activate_project(root.path().to_path_buf())
            .await
            .expect("missing optional server must not fail activation");
        assert_eq!(activation.health(), ActivationHealth::StructuralOnly);
    }
}
