//! Lossless semantic-edit requests and bounded structural fallbacks.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use lsp_types::{
    DidCloseTextDocumentParams, DocumentFormattingParams, DocumentRangeFormattingParams,
    FormattingOptions, GotoDefinitionParams, InsertTextFormat, PartialResultParams,
    RenameParams as LspRenameParams, TextDocumentIdentifier, TextDocumentPositionParams,
    WorkDoneProgressParams, WorkspaceEdit,
};
use serde::{Deserialize, Serialize};

use super::Translator;
use super::dto::{
    CodeAction, CommandDescription, Diagnostic, DiagnosticSeverity, DocumentChanges, Location,
    Position2D, Range, TextEdit, WorkspaceEditDescription,
};
use crate::bridge::ast_grep;
use crate::bridge::state::detect_language;
use crate::bridge::{PositionEncoding, lock_std, path_to_uri, uri_to_path};
use crate::config::ToolKind;
use crate::edit_paths::FileOperation;
use crate::error::{Error, Result};

const MAX_DISCOVERY_ITEMS: usize = 100;
const MAX_DISCOVERY_BYTES: usize = 1024 * 1024;
const PROVIDER_SYNC_STABILITY_WINDOW: Duration = Duration::from_millis(100);

#[derive(Debug, Clone)]
/// Edits returned by language servers participating in a file rename.
pub struct WillRenameFilesResult {
    /// Server identities that accepted the rename notification.
    pub providers: Vec<String>,
    /// Non-empty workspace edits returned by those providers.
    pub edits: Vec<WorkspaceEdit>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
/// Post-commit convergence status for one language server.
pub struct ProviderSynchronization {
    /// Stable routing identity of the provider.
    pub provider: String,
    /// Whether watched files, document lifecycle, and VFS probes converged.
    pub synchronized: bool,
    /// Number of watched-file notifications flushed to the provider.
    pub watched_file_notifications: usize,
    /// Failure or degradation detail when synchronization was not proven.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone)]
/// Capability result and optional edit for a local semantic operation.
pub struct SupportedWorkspaceEdit {
    /// Whether the selected server advertises the operation.
    pub supported: bool,
    /// Non-empty edit returned by the server, when any.
    pub edit: Option<WorkspaceEdit>,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
/// Semantic discovery operation requested from the active Rust analyzer.
pub enum SemanticDiscoveryKind {
    /// Standard declaration lookup.
    Declaration,
    /// rust-analyzer parent-module lookup.
    ParentModule,
    /// rust-analyzer child-module lookup.
    ChildModules,
    /// rust-analyzer macro expansion.
    MacroExpansion,
    /// Standard nested selection ranges.
    SelectionRanges,
    /// rust-analyzer runnable discovery.
    Runnables,
    /// rust-analyzer related-test discovery.
    RelatedTests,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MacroExpansion {
    pub name: String,
    pub expansion: String,
}

#[derive(Debug, Clone, Serialize)]
/// Bounded result from one semantic discovery operation.
pub struct SemanticDiscoveryResult {
    /// Whether the active server supports the operation.
    pub supported: bool,
    /// Stable name of the protocol provider.
    pub provider: String,
    /// Semantic relationship represented by location results.
    pub kind: SemanticDiscoveryKind,
    /// Locations returned by declaration or module discovery.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub locations: Vec<Location>,
    /// Inner-to-outer ranges returned by selection discovery.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub selection_ranges: Vec<Range>,
    /// Expansion returned by macro discovery.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub macro_expansion: Option<MacroExpansion>,
    /// Raw, redacted runnable or related-test payloads.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub runnables: Vec<serde_json::Value>,
    /// Whether configured item or byte bounds truncated the response.
    pub truncated: bool,
}

impl SemanticDiscoveryResult {
    fn empty(supported: bool, provider: &str, kind: SemanticDiscoveryKind) -> Self {
        Self {
            supported,
            provider: provider.to_string(),
            kind,
            locations: Vec::new(),
            selection_ranges: Vec::new(),
            macro_expansion: None,
            runnables: Vec::new(),
            truncated: false,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SnippetTextEdit {
    range: lsp_types::Range,
    new_text: String,
    insert_text_format: Option<InsertTextFormat>,
}

#[derive(Debug, Clone, Copy, Serialize)]
enum MoveItemDirection {
    Up,
    Down,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct MoveItemParams {
    direction: MoveItemDirection,
    text_document: TextDocumentIdentifier,
    range: lsp_types::Range,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RustAnalyzerSsrParams {
    query: String,
    parse_only: bool,
    #[serde(flatten)]
    position: TextDocumentPositionParams,
    selections: Vec<lsp_types::Range>,
}

impl Translator {
    #[must_use]
    pub(crate) fn semantic_server_ready_for_file(&self, path: &Path) -> bool {
        let language = detect_language(path, &self.extension_map);
        let configs = lock_std(&self.project_lsp_configs);
        let clients = lock_std(&self.lsp_clients);
        let expected = lock_std(&self.expected_servers);
        configs.iter().any(|config| {
            let id = config.id();
            config.language_id.eq_ignore_ascii_case(&language)
                && clients.contains_key(&id)
                && !expected.contains(&id)
        })
    }

    pub(crate) async fn request_will_rename_files(
        &self,
        old_path: &Path,
        new_path: &Path,
    ) -> Result<WillRenameFilesResult> {
        let clients = lock_std(&self.lsp_clients).clone();
        let mut providers = lock_std(&self.lsp_servers)
            .iter()
            .filter(|(id, server)| {
                clients.contains_key(*id)
                    && server
                        .capabilities()
                        .workspace
                        .as_ref()
                        .and_then(|workspace| workspace.file_operations.as_ref())
                        .and_then(|operations| operations.will_rename.as_ref())
                        .is_some_and(|registration| {
                            file_operation_registration_matches(registration, old_path)
                        })
            })
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        providers.sort_by(|left, right| left.as_str().cmp(right.as_str()));
        let params = lsp_types::RenameFilesParams {
            files: vec![lsp_types::FileRename {
                old_uri: path_to_uri(old_path)?.to_string(),
                new_uri: path_to_uri(new_path)?.to_string(),
            }],
        };
        let mut edits = Vec::new();
        for id in &providers {
            let Some(client) = clients.get(id) else {
                continue;
            };
            if let Some(edit) = client
                .request::<_, Option<WorkspaceEdit>>(
                    "workspace/willRenameFiles",
                    params.clone(),
                    client.request_timeout(),
                )
                .await?
            {
                edits.push(edit);
            }
        }
        Ok(WillRenameFilesResult {
            providers: providers.into_iter().map(|id| id.to_string()).collect(),
            edits,
        })
    }

    /// Flush dynamic file watchers and probe each provider's VFS for convergence.
    #[allow(clippy::too_many_lines)]
    pub(crate) async fn synchronize_resource_operations(
        &self,
        operations: &[FileOperation],
    ) -> Vec<ProviderSynchronization> {
        if operations.is_empty() {
            return Vec::new();
        }
        let lifecycle_errors = self.reconcile_resource_documents(operations).await;
        let clients = lock_std(&self.lsp_clients).clone();
        let configs = lock_std(&self.project_lsp_configs)
            .iter()
            .map(|config| {
                (
                    config.id(),
                    (
                        config.language_id.clone(),
                        Duration::from_secs(config.request_timeout_seconds.clamp(1, 5)),
                    ),
                )
            })
            .collect::<std::collections::HashMap<_, _>>();
        let probe_capabilities = lock_std(&self.lsp_servers)
            .iter()
            .map(|(id, server)| {
                (
                    id.clone(),
                    (
                        server
                            .capabilities()
                            .document_symbol_provider
                            .as_ref()
                            .is_some_and(one_of_enabled),
                        server.is_rust_analyzer()
                            && experimental_enabled(server.capabilities(), "parentModule"),
                    ),
                )
            })
            .collect::<std::collections::HashMap<_, _>>();
        let mut ids = clients.keys().cloned().collect::<Vec<_>>();
        ids.sort_by(|left, right| left.as_str().cmp(right.as_str()));
        let mut results = Vec::new();
        for id in ids {
            let client = &clients[&id];
            let Some((language, timeout)) = configs.get(&id) else {
                continue;
            };
            let probes = resource_sync_probes(operations, language, &self.extension_map);
            if probes.is_empty() {
                continue;
            }
            let (registrations, notifications) =
                match client.synchronize_watched_files(&[], *timeout).await {
                    Ok(counts) => counts,
                    Err(error) => {
                        results.push(ProviderSynchronization {
                            provider: id.to_string(),
                            synchronized: false,
                            watched_file_notifications: 0,
                            message: Some(format!("watched-file synchronization failed: {error}")),
                        });
                        continue;
                    }
                };
            if registrations == 0 {
                results.push(ProviderSynchronization {
                    provider: id.to_string(),
                    synchronized: false,
                    watched_file_notifications: notifications,
                    message: Some(
                        "provider has no dynamic workspace/didChangeWatchedFiles registration"
                            .to_string(),
                    ),
                });
                continue;
            }
            let (supports_document_symbols, supports_parent_module) =
                probe_capabilities.get(&id).copied().unwrap_or_default();
            if !supports_document_symbols {
                results.push(ProviderSynchronization {
                    provider: id.to_string(),
                    synchronized: false,
                    watched_file_notifications: notifications,
                    message: Some(
                        "provider cannot prove VFS convergence without document symbols"
                            .to_string(),
                    ),
                });
                continue;
            }
            if let Some(error) = lifecycle_errors.get(&id) {
                results.push(ProviderSynchronization {
                    provider: id.to_string(),
                    synchronized: false,
                    watched_file_notifications: notifications,
                    message: Some(format!(
                        "document lifecycle synchronization failed: {error}"
                    )),
                });
                continue;
            }
            let mut synchronized = true;
            let mut message = None;
            for probe in probes {
                let params = lsp_types::DocumentSymbolParams {
                    text_document: TextDocumentIdentifier {
                        uri: match path_to_uri(&probe.path) {
                            Ok(uri) => uri,
                            Err(error) => {
                                synchronized = false;
                                message = Some(format!(
                                    "provider VFS probe path failed for {}: {error}",
                                    probe.path.display()
                                ));
                                break;
                            }
                        },
                    },
                    work_done_progress_params: WorkDoneProgressParams::default(),
                    partial_result_params: PartialResultParams::default(),
                };
                let mut converged = false;
                let mut stable_since = None;
                for _ in 0..128 {
                    let response: Result<serde_json::Value> = client
                        .request("textDocument/documentSymbol", params.clone(), *timeout)
                        .await;
                    let mut matches_expected_state = match response {
                        Ok(value) => value.is_null() != probe.expect_present,
                        Err(error)
                            if !probe.expect_present
                                && error_indicates_missing_document(&error) =>
                        {
                            true
                        }
                        Err(error) => {
                            message = Some(format!(
                                "provider VFS probe failed for {}: {error}",
                                probe.path.display()
                            ));
                            break;
                        }
                    };
                    if matches_expected_state && probe.expect_present && supports_parent_module {
                        let parent_params = TextDocumentPositionParams {
                            text_document: params.text_document.clone(),
                            position: lsp_types::Position::new(0, 0),
                        };
                        match client
                            .request::<_, serde_json::Value>(
                                "experimental/parentModule",
                                parent_params,
                                *timeout,
                            )
                            .await
                        {
                            Ok(value) => {
                                matches_expected_state = !value.is_null()
                                    && value
                                        .as_array()
                                        .is_none_or(|locations| !locations.is_empty());
                            }
                            Err(error) => {
                                message = Some(format!(
                                    "provider semantic probe failed for {}: {error}",
                                    probe.path.display()
                                ));
                                break;
                            }
                        }
                    }
                    if matches_expected_state {
                        let since = *stable_since.get_or_insert_with(Instant::now);
                        if since.elapsed() >= PROVIDER_SYNC_STABILITY_WINDOW {
                            converged = true;
                            break;
                        }
                    } else {
                        stable_since = None;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                if !converged {
                    synchronized = false;
                    message.get_or_insert_with(|| {
                        format!("provider VFS did not converge for {}", probe.path.display())
                    });
                    break;
                }
            }
            results.push(ProviderSynchronization {
                provider: id.to_string(),
                synchronized,
                watched_file_notifications: notifications,
                message,
            });
        }
        results
    }

    /// Flush watched-file changes and prove exact post-commit text for each provider.
    #[allow(clippy::too_many_lines)]
    pub(crate) async fn synchronize_text_changes(
        &self,
        changes: &[(PathBuf, String)],
    ) -> Vec<ProviderSynchronization> {
        if changes.is_empty() {
            return Vec::new();
        }
        let clients = lock_std(&self.lsp_clients).clone();
        let configs = lock_std(&self.project_lsp_configs)
            .iter()
            .map(|config| {
                (
                    config.id(),
                    (
                        config.language_id.clone(),
                        Duration::from_secs(config.request_timeout_seconds.clamp(1, 5)),
                    ),
                )
            })
            .collect::<std::collections::HashMap<_, _>>();
        let rust_analyzer = lock_std(&self.lsp_servers)
            .iter()
            .map(|(id, server)| (id.clone(), server.is_rust_analyzer()))
            .collect::<std::collections::HashMap<_, _>>();
        let mut ids = clients.keys().cloned().collect::<Vec<_>>();
        ids.sort_by(|left, right| left.as_str().cmp(right.as_str()));
        let mut results = Vec::new();
        for id in ids {
            let client = &clients[&id];
            let Some((language, timeout)) = configs.get(&id) else {
                continue;
            };
            let provider_changes = changes
                .iter()
                .filter(|(path, _)| {
                    path.is_file() && detect_language(path, &self.extension_map) == *language
                })
                .collect::<Vec<_>>();
            if provider_changes.is_empty() {
                continue;
            }
            let changed_paths = provider_changes
                .iter()
                .map(|(path, _)| path.clone())
                .collect::<Vec<_>>();
            let (registrations, notifications) = match client
                .synchronize_watched_files(&changed_paths, *timeout)
                .await
            {
                Ok(counts) => counts,
                Err(error) => {
                    results.push(ProviderSynchronization {
                        provider: id.to_string(),
                        synchronized: false,
                        watched_file_notifications: 0,
                        message: Some(format!("watched-file synchronization failed: {error}")),
                    });
                    continue;
                }
            };
            if registrations == 0 {
                results.push(ProviderSynchronization {
                    provider: id.to_string(),
                    synchronized: false,
                    watched_file_notifications: notifications,
                    message: Some(
                        "provider has no dynamic workspace/didChangeWatchedFiles registration"
                            .to_string(),
                    ),
                });
                continue;
            }
            if !rust_analyzer.get(&id).copied().unwrap_or_default() {
                results.push(ProviderSynchronization {
                    provider: id.to_string(),
                    synchronized: false,
                    watched_file_notifications: notifications,
                    message: Some("provider cannot prove exact post-edit file content".to_string()),
                });
                continue;
            }
            let mut synchronized = true;
            let mut message = None;
            for (path, expected) in provider_changes {
                let params = match path_to_uri(path) {
                    Ok(uri) => TextDocumentIdentifier { uri },
                    Err(error) => {
                        synchronized = false;
                        message = Some(format!(
                            "provider text probe path failed for {}: {error}",
                            path.display()
                        ));
                        break;
                    }
                };
                let mut stable_since = None;
                let mut converged = false;
                for _ in 0..128 {
                    match client
                        .request::<_, String>(
                            "rust-analyzer/viewFileText",
                            params.clone(),
                            *timeout,
                        )
                        .await
                    {
                        Ok(actual) if actual == *expected => {
                            let since = *stable_since.get_or_insert_with(Instant::now);
                            if since.elapsed() >= PROVIDER_SYNC_STABILITY_WINDOW {
                                converged = true;
                                break;
                            }
                        }
                        Ok(_) => stable_since = None,
                        Err(error) => {
                            message = Some(format!(
                                "provider text probe failed for {}: {error}",
                                path.display()
                            ));
                            break;
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                if !converged {
                    synchronized = false;
                    message.get_or_insert_with(|| {
                        format!("provider file text did not converge for {}", path.display())
                    });
                    break;
                }
            }
            results.push(ProviderSynchronization {
                provider: id.to_string(),
                synchronized,
                watched_file_notifications: notifications,
                message,
            });
        }
        results
    }

    async fn reconcile_resource_documents(
        &self,
        operations: &[FileOperation],
    ) -> std::collections::HashMap<crate::config::ServerId, String> {
        let transitions = self
            .document_tracker
            .open_documents()
            .into_iter()
            .filter_map(|document| {
                let source = uri_to_path(document.uri())?;
                let destination = resource_document_destination(&source, operations)?;
                Some((source, document, destination))
            })
            .collect::<Vec<_>>();
        let mut errors = std::collections::HashMap::new();

        // Close every old URI first. An overwriting rename can otherwise
        // close the source after it has already been reopened at its target.
        for (source, document, _) in &transitions {
            self.document_tracker.close(source);
            for (id, client) in self.clients_for_language(document.language_id()) {
                if document.synced_version(&id).is_none() {
                    continue;
                }
                let params = DidCloseTextDocumentParams {
                    text_document: TextDocumentIdentifier {
                        uri: document.uri().clone(),
                    },
                };
                if let Err(error) = client.notify("textDocument/didClose", params).await {
                    errors.entry(id).or_insert_with(|| error.to_string());
                }
            }
        }

        for (_, document, destination) in transitions {
            let ResourceDocumentChange::Moved(destination) = destination else {
                continue;
            };
            let language = detect_language(&destination, &self.extension_map);
            if let Err(error) = self
                .document_tracker
                .open(destination.clone(), document.content().to_string())
            {
                for (id, _) in self.clients_for_language(&language) {
                    errors.entry(id).or_insert_with(|| error.to_string());
                }
                continue;
            }
            for (id, client) in self.clients_for_language(&language) {
                if let Err(error) = self
                    .document_tracker
                    .sync_tracked(&destination, &id, &client)
                    .await
                {
                    errors.entry(id).or_insert_with(|| error.to_string());
                }
            }
        }
        errors
    }

    pub(crate) async fn structural_ast_grep_search(
        &self,
        root: PathBuf,
        language: String,
        query: String,
        replacement: Option<String>,
        encoding: PositionEncoding,
        parse_only: bool,
    ) -> std::result::Result<ast_grep::StructuralSearchResult, String> {
        let overrides = self
            .document_tracker
            .open_documents()
            .into_iter()
            .filter_map(|document| {
                uri_to_path(document.uri()).map(|path| (path, document.content().to_string()))
            })
            .collect();
        ast_grep::structural_search(
            root,
            language,
            query,
            replacement,
            encoding,
            overrides,
            parse_only,
        )
        .await
    }

    /// Request a lossless standard LSP rename edit.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid paths, unsupported servers, or request failure.
    pub async fn request_rename_workspace_edit(
        &self,
        file_path: String,
        line: u32,
        character: u32,
        new_name: String,
    ) -> Result<Option<WorkspaceEdit>> {
        let (id, client, uri) = self
            .prepare_gated_document(&file_path, ToolKind::Rename, "renameProvider", |caps| {
                matches!(
                    caps.rename_provider,
                    Some(lsp_types::OneOf::Left(true) | lsp_types::OneOf::Right(_))
                )
            })
            .await?;
        let position = self.encoding_ctx(&id).to_lsp(&uri, line, character).await;
        client
            .request(
                "textDocument/rename",
                LspRenameParams {
                    text_document_position: TextDocumentPositionParams {
                        text_document: TextDocumentIdentifier { uri },
                        position,
                    },
                    new_name,
                    work_done_progress_params: WorkDoneProgressParams::default(),
                },
                client.request_timeout(),
            )
            .await
    }

    pub(crate) async fn request_rust_analyzer_ssr(
        &self,
        file_path: String,
        query: String,
        parse_only: bool,
    ) -> Result<WorkspaceEdit> {
        let (id, client, uri) = self
            .prepare_document(&file_path, ToolKind::CodeActions)
            .await?;
        let supported = lock_std(&self.lsp_servers)
            .get(&id)
            .and_then(|server| server.capabilities().experimental.as_ref())
            .and_then(|value| value.get("ssr"))
            .and_then(serde_json::Value::as_bool)
            == Some(true);
        if !supported {
            return Err(Error::InvalidToolParams(
                "active Rust language server does not advertise experimental SSR support"
                    .to_string(),
            ));
        }
        client
            .request(
                "experimental/ssr",
                RustAnalyzerSsrParams {
                    query,
                    parse_only,
                    position: TextDocumentPositionParams {
                        text_document: TextDocumentIdentifier { uri },
                        position: lsp_types::Position::new(0, 0),
                    },
                    selections: Vec::new(),
                },
                client.request_timeout(),
            )
            .await
    }

    /// Request a lossless standard LSP document-formatting edit.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid paths, unsupported servers, or request failure.
    pub async fn request_format_workspace_edit(
        &self,
        file_path: String,
        tab_size: u32,
        insert_spaces: bool,
    ) -> Result<Option<WorkspaceEdit>> {
        let (_, client, uri) = self
            .prepare_gated_document(
                &file_path,
                ToolKind::FormatDocument,
                "documentFormattingProvider",
                |caps| {
                    matches!(
                        caps.document_formatting_provider,
                        Some(lsp_types::OneOf::Left(true) | lsp_types::OneOf::Right(_))
                    )
                },
            )
            .await?;
        let edits: Option<Vec<lsp_types::TextEdit>> = client
            .request(
                "textDocument/formatting",
                DocumentFormattingParams {
                    text_document: TextDocumentIdentifier { uri: uri.clone() },
                    options: FormattingOptions {
                        tab_size,
                        insert_spaces,
                        ..Default::default()
                    },
                    work_done_progress_params: WorkDoneProgressParams::default(),
                },
                client.request_timeout(),
            )
            .await?;
        Ok(edits.map(|edits| workspace_edit_for(uri, edits)))
    }

    pub(crate) async fn request_range_format_workspace_edit(
        &self,
        file_path: String,
        start: (u32, u32),
        end: (u32, u32),
        tab_size: u32,
        insert_spaces: bool,
    ) -> Result<SupportedWorkspaceEdit> {
        let prepared = self
            .prepare_gated_document(
                &file_path,
                ToolKind::FormatDocument,
                "documentRangeFormattingProvider",
                |caps| {
                    matches!(
                        caps.document_range_formatting_provider,
                        Some(lsp_types::OneOf::Left(true) | lsp_types::OneOf::Right(_))
                    )
                },
            )
            .await;
        let (id, client, uri) = match prepared {
            Err(Error::CapabilityNotSupported { .. }) => {
                return Ok(SupportedWorkspaceEdit {
                    supported: false,
                    edit: None,
                });
            }
            value => value?,
        };
        let ctx = self.encoding_ctx(&id);
        let range = lsp_types::Range {
            start: ctx.to_lsp(&uri, start.0, start.1).await,
            end: ctx.to_lsp(&uri, end.0, end.1).await,
        };
        if range.start > range.end {
            return Err(Error::InvalidToolParams(
                "formatting range start must not follow its end".to_string(),
            ));
        }
        let edits: Option<Vec<lsp_types::TextEdit>> = client
            .request(
                "textDocument/rangeFormatting",
                DocumentRangeFormattingParams {
                    text_document: TextDocumentIdentifier { uri: uri.clone() },
                    range,
                    options: FormattingOptions {
                        tab_size,
                        insert_spaces,
                        ..Default::default()
                    },
                    work_done_progress_params: WorkDoneProgressParams::default(),
                },
                client.request_timeout(),
            )
            .await?;
        Ok(SupportedWorkspaceEdit {
            supported: true,
            edit: edits
                .filter(|edits| !edits.is_empty())
                .map(|edits| workspace_edit_for(uri, edits)),
        })
    }

    pub(crate) async fn request_move_item_workspace_edit(
        &self,
        file_path: String,
        start: (u32, u32),
        end: (u32, u32),
        direction: &str,
    ) -> Result<SupportedWorkspaceEdit> {
        let (id, client, uri) = self
            .prepare_document(&file_path, ToolKind::CodeActions)
            .await?;
        let supported = lock_std(&self.lsp_servers)
            .get(&id)
            .and_then(|server| server.capabilities().experimental.as_ref())
            .and_then(|value| value.get("moveItem"))
            .and_then(serde_json::Value::as_bool)
            == Some(true);
        if !supported {
            return Ok(SupportedWorkspaceEdit {
                supported: false,
                edit: None,
            });
        }
        let direction = match direction {
            "up" => MoveItemDirection::Up,
            "down" => MoveItemDirection::Down,
            value => {
                return Err(Error::InvalidToolParams(format!(
                    "unsupported move direction {value:?}; expected up or down"
                )));
            }
        };
        let ctx = self.encoding_ctx(&id);
        let range = lsp_types::Range {
            start: ctx.to_lsp(&uri, start.0, start.1).await,
            end: ctx.to_lsp(&uri, end.0, end.1).await,
        };
        if range.start > range.end {
            return Err(Error::InvalidToolParams(
                "move range start must not follow its end".to_string(),
            ));
        }
        let edits: Vec<SnippetTextEdit> = client
            .request(
                "experimental/moveItem",
                MoveItemParams {
                    direction,
                    text_document: TextDocumentIdentifier { uri: uri.clone() },
                    range,
                },
                client.request_timeout(),
            )
            .await?;
        let edits = edits
            .into_iter()
            .map(|edit| {
                let new_text = if edit.insert_text_format == Some(InsertTextFormat::SNIPPET) {
                    plain_text_snippet(&edit.new_text).ok_or_else(|| {
                        Error::InvalidToolParams(
                            "move-item edit contains unresolved snippet placeholders".to_string(),
                        )
                    })?
                } else {
                    edit.new_text
                };
                Ok(lsp_types::TextEdit {
                    range: edit.range,
                    new_text,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(SupportedWorkspaceEdit {
            supported: true,
            edit: (!edits.is_empty()).then(|| workspace_edit_for(uri, edits)),
        })
    }

    /// Request raw code actions without discarding edit, disabled, or data fields.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid ranges, routing failures, or request failure.
    pub async fn request_code_actions(
        &self,
        file_path: String,
        start_line: u32,
        start_character: u32,
        end_line: u32,
        end_character: u32,
        kind_filter: Option<String>,
    ) -> Result<Vec<lsp_types::CodeActionOrCommand>> {
        validate_code_action_params(start_line, start_character, end_line, end_character)?;
        let (id, client, uri) = self
            .prepare_gated_document(
                &file_path,
                ToolKind::CodeActions,
                "codeActionProvider",
                |caps| {
                    matches!(
                        caps.code_action_provider,
                        Some(
                            lsp_types::CodeActionProviderCapability::Simple(true)
                                | lsp_types::CodeActionProviderCapability::Options(_)
                        )
                    )
                },
            )
            .await?;
        let ctx = self.encoding_ctx(&id);
        let response: Option<lsp_types::CodeActionResponse> = client
            .request(
                "textDocument/codeAction",
                lsp_types::CodeActionParams {
                    text_document: TextDocumentIdentifier { uri: uri.clone() },
                    range: lsp_types::Range {
                        start: ctx.to_lsp(&uri, start_line, start_character).await,
                        end: ctx.to_lsp(&uri, end_line, end_character).await,
                    },
                    context: lsp_types::CodeActionContext {
                        diagnostics: Vec::new(),
                        only: kind_filter.map(|kind| vec![lsp_types::CodeActionKind::from(kind)]),
                        trigger_kind: Some(lsp_types::CodeActionTriggerKind::INVOKED),
                    },
                    work_done_progress_params: WorkDoneProgressParams::default(),
                    partial_result_params: PartialResultParams::default(),
                },
                client.request_timeout(),
            )
            .await?;
        Ok(response.unwrap_or_default())
    }

    /// Resolve a previously listed code action through its originating server.
    ///
    /// # Errors
    ///
    /// Returns an error when the file cannot be routed or resolution fails.
    pub async fn resolve_code_action(
        &self,
        file_path: &str,
        action: lsp_types::CodeAction,
    ) -> Result<lsp_types::CodeAction> {
        let (_, client, _) = self
            .prepare_document(file_path, ToolKind::CodeActions)
            .await?;
        client
            .request("codeAction/resolve", action, client.request_timeout())
            .await
    }

    #[allow(clippy::too_many_lines)]
    pub(crate) async fn request_semantic_discovery(
        &self,
        file_path: String,
        line: u32,
        character: u32,
        kind: SemanticDiscoveryKind,
    ) -> Result<SemanticDiscoveryResult> {
        let (id, client, uri) = self
            .prepare_document(&file_path, ToolKind::Definition)
            .await?;
        let (provider, supported) = {
            let servers = lock_std(&self.lsp_servers);
            let server = servers
                .get(&id)
                .ok_or_else(|| Error::NoServerForLanguage(id.to_string()))?;
            let capabilities = server.capabilities();
            let decision = match kind {
            SemanticDiscoveryKind::Declaration => (
                "standard_lsp",
                capabilities.declaration_provider.as_ref().is_some_and(|capability| {
                    matches!(capability, lsp_types::DeclarationCapability::Simple(true)
                        | lsp_types::DeclarationCapability::RegistrationOptions(_))
                }),
            ),
            SemanticDiscoveryKind::SelectionRanges => (
                "standard_lsp",
                capabilities.selection_range_provider.as_ref().is_some_and(|capability| {
                    matches!(capability, lsp_types::SelectionRangeProviderCapability::Simple(true)
                        | lsp_types::SelectionRangeProviderCapability::RegistrationOptions(_))
                }),
            ),
            SemanticDiscoveryKind::ParentModule => (
                "rust_analyzer",
                experimental_enabled(capabilities, "parentModule"),
            ),
            SemanticDiscoveryKind::ChildModules => (
                "rust_analyzer",
                experimental_enabled(capabilities, "childModules"),
            ),
            SemanticDiscoveryKind::Runnables => (
                "rust_analyzer",
                experimental_enabled(capabilities, "runnables"),
            ),
                SemanticDiscoveryKind::MacroExpansion | SemanticDiscoveryKind::RelatedTests => {
                    ("rust_analyzer", server.is_rust_analyzer())
                }
            };
            drop(servers);
            decision
        };
        if !supported {
            return Ok(SemanticDiscoveryResult::empty(false, provider, kind));
        }
        let ctx = self.encoding_ctx(&id);
        let position = ctx.to_lsp(&uri, line, character).await;
        let position_params = TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri: uri.clone() },
            position,
        };
        let timeout = client.request_timeout();
        let mut result = SemanticDiscoveryResult::empty(true, provider, kind);
        match kind {
            SemanticDiscoveryKind::Declaration => {
                let response: Option<lsp_types::GotoDefinitionResponse> = client
                    .request(
                        "textDocument/declaration",
                        GotoDefinitionParams {
                            text_document_position_params: position_params,
                            work_done_progress_params: WorkDoneProgressParams::default(),
                            partial_result_params: PartialResultParams::default(),
                        },
                        timeout,
                    )
                    .await?;
                (result.locations, result.truncated) = super::navigation::bounded_locations(
                    response,
                    &ctx,
                    &self.workspace_roots,
                    MAX_DISCOVERY_ITEMS,
                )
                .await;
            }
            SemanticDiscoveryKind::ParentModule | SemanticDiscoveryKind::ChildModules => {
                let method = if matches!(kind, SemanticDiscoveryKind::ParentModule) {
                    "experimental/parentModule"
                } else {
                    "experimental/childModules"
                };
                let response: Option<lsp_types::GotoDefinitionResponse> =
                    client.request(method, position_params, timeout).await?;
                (result.locations, result.truncated) = super::navigation::bounded_locations(
                    response,
                    &ctx,
                    &self.workspace_roots,
                    MAX_DISCOVERY_ITEMS,
                )
                .await;
            }
            SemanticDiscoveryKind::MacroExpansion => {
                if let Some(mut expansion) = client
                    .request::<_, Option<MacroExpansion>>(
                        "rust-analyzer/expandMacro",
                        position_params,
                        timeout,
                    )
                    .await?
                {
                    result.truncated = truncate_utf8(&mut expansion.name, MAX_DISCOVERY_BYTES);
                    let remaining = MAX_DISCOVERY_BYTES.saturating_sub(expansion.name.len());
                    result.truncated |= truncate_utf8(&mut expansion.expansion, remaining);
                    result.macro_expansion = Some(expansion);
                }
            }
            SemanticDiscoveryKind::SelectionRanges => {
                let response: Option<Vec<lsp_types::SelectionRange>> = client
                    .request(
                        "textDocument/selectionRange",
                        lsp_types::SelectionRangeParams {
                            text_document: TextDocumentIdentifier { uri },
                            positions: vec![position],
                            work_done_progress_params: WorkDoneProgressParams::default(),
                            partial_result_params: PartialResultParams::default(),
                        },
                        timeout,
                    )
                    .await?;
                let mut current = response.and_then(|ranges| ranges.into_iter().next());
                while let Some(selection) = current {
                    if result.selection_ranges.len() == MAX_DISCOVERY_ITEMS {
                        result.truncated = true;
                        break;
                    }
                    result
                        .selection_ranges
                        .push(normalize_range(selection.range));
                    current = selection.parent.map(|parent| *parent);
                }
            }
            SemanticDiscoveryKind::Runnables => {
                let mut values: Vec<serde_json::Value> = client
                    .request(
                        "experimental/runnables",
                        serde_json::json!({"textDocument": {"uri": uri}, "position": position}),
                        timeout,
                    )
                    .await?;
                for value in &mut values {
                    self.redaction_policy.redact_json(value);
                }
                (result.runnables, result.truncated) = bounded_json_values(values);
            }
            SemanticDiscoveryKind::RelatedTests => {
                let mut values: Vec<serde_json::Value> = client
                    .request("rust-analyzer/relatedTests", position_params, timeout)
                    .await?;
                for value in &mut values {
                    self.redaction_policy.redact_json(value);
                }
                (result.runnables, result.truncated) = bounded_json_values(values);
            }
        }
        Ok(result)
    }
}

/// Convert a lossless LSP code action or command to the MCP DTO.
#[must_use]
pub fn convert_code_action_or_command(
    action: lsp_types::CodeActionOrCommand,
    action_id: Option<String>,
) -> CodeAction {
    match action {
        lsp_types::CodeActionOrCommand::CodeAction(action) => {
            let workspace_edit = action
                .edit
                .as_ref()
                .and_then(|edit| serde_json::to_value(edit).ok());
            let edit = action.edit.clone().map(workspace_edit_description);
            CodeAction {
                action_id,
                title: action.title,
                kind: action.kind.map(|kind| kind.as_str().to_string()),
                diagnostics: action
                    .diagnostics
                    .unwrap_or_default()
                    .into_iter()
                    .map(convert_diagnostic)
                    .collect(),
                edit,
                workspace_edit,
                command: action.command.map(convert_command),
                is_preferred: action.is_preferred.unwrap_or(false),
                disabled: action.disabled.map(|disabled| disabled.reason),
                data: action.data,
            }
        }
        lsp_types::CodeActionOrCommand::Command(command) => CodeAction {
            action_id,
            title: command.title.clone(),
            kind: None,
            diagnostics: Vec::new(),
            edit: None,
            workspace_edit: None,
            command: Some(convert_command(command)),
            is_preferred: false,
            disabled: None,
            data: None,
        },
    }
}

fn workspace_edit_for(uri: lsp_types::Uri, edits: Vec<lsp_types::TextEdit>) -> WorkspaceEdit {
    WorkspaceEdit {
        changes: Some(std::iter::once((uri, edits)).collect()),
        document_changes: None,
        change_annotations: None,
    }
}

fn validate_code_action_params(
    start_line: u32,
    start_character: u32,
    end_line: u32,
    end_character: u32,
) -> Result<()> {
    if [start_line, start_character, end_line, end_character].contains(&0) {
        return Err(Error::InvalidToolParams(
            "line and character positions must be >= 1".to_string(),
        ));
    }
    if (start_line, start_character) > (end_line, end_character) {
        return Err(Error::InvalidToolParams(
            "start position must not follow end position".to_string(),
        ));
    }
    Ok(())
}

fn experimental_enabled(capabilities: &lsp_types::ServerCapabilities, key: &str) -> bool {
    !matches!(
        capabilities
            .experimental
            .as_ref()
            .and_then(|value| value.get(key)),
        None | Some(serde_json::Value::Null | serde_json::Value::Bool(false))
    )
}

fn bounded_json_values(values: Vec<serde_json::Value>) -> (Vec<serde_json::Value>, bool) {
    let mut retained = Vec::new();
    let mut bytes = 0usize;
    let mut truncated = values.len() > MAX_DISCOVERY_ITEMS;
    for value in values.into_iter().take(MAX_DISCOVERY_ITEMS) {
        let size = serde_json::to_vec(&value).map_or(MAX_DISCOVERY_BYTES + 1, |value| value.len());
        if bytes.saturating_add(size) > MAX_DISCOVERY_BYTES {
            truncated = true;
            break;
        }
        bytes += size;
        retained.push(value);
    }
    (retained, truncated)
}

fn truncate_utf8(value: &mut String, max_bytes: usize) -> bool {
    if value.len() <= max_bytes {
        return false;
    }
    let mut boundary = max_bytes.min(value.len());
    while !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    value.truncate(boundary);
    true
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

fn convert_diagnostic(diagnostic: lsp_types::Diagnostic) -> Diagnostic {
    Diagnostic {
        range: normalize_range(diagnostic.range),
        severity: match diagnostic.severity {
            Some(lsp_types::DiagnosticSeverity::ERROR) => DiagnosticSeverity::Error,
            Some(lsp_types::DiagnosticSeverity::WARNING) => DiagnosticSeverity::Warning,
            Some(lsp_types::DiagnosticSeverity::HINT) => DiagnosticSeverity::Hint,
            _ => DiagnosticSeverity::Information,
        },
        message: diagnostic.message,
        code: diagnostic.code.map(|code| match code {
            lsp_types::NumberOrString::Number(value) => value.to_string(),
            lsp_types::NumberOrString::String(value) => value,
        }),
    }
}

fn convert_command(command: lsp_types::Command) -> CommandDescription {
    CommandDescription {
        title: command.title,
        command: command.command,
        arguments: command.arguments.unwrap_or_default(),
    }
}

fn workspace_edit_description(edit: WorkspaceEdit) -> WorkspaceEditDescription {
    let changes = edit.changes.map_or_else(Vec::new, |changes| {
        changes
            .into_iter()
            .map(|(uri, edits)| DocumentChanges {
                uri: uri.to_string(),
                edits: edits
                    .into_iter()
                    .map(|edit| TextEdit {
                        range: normalize_range(edit.range),
                        new_text: edit.new_text,
                    })
                    .collect(),
            })
            .collect()
    });
    WorkspaceEditDescription { changes }
}

fn plain_text_snippet(snippet: &str) -> Option<String> {
    let mut output = String::with_capacity(snippet.len());
    let mut chars = snippet.chars().peekable();
    while let Some(character) = chars.next() {
        if character == '\\' {
            output.push(chars.next()?);
        } else if character == '$' {
            match chars.peek() {
                Some('$') => {
                    chars.next();
                    output.push('$');
                }
                Some('0') => {
                    chars.next();
                }
                Some('{') => {
                    chars.next();
                    if chars.next() != Some('0') || chars.next() != Some('}') {
                        return None;
                    }
                }
                Some('1'..='9') => return None,
                _ => output.push(character),
            }
        } else {
            output.push(character);
        }
    }
    Some(output)
}

fn file_operation_registration_matches(
    registration: &lsp_types::FileOperationRegistrationOptions,
    path: &Path,
) -> bool {
    registration.filters.iter().any(|filter| {
        let matches_scheme = filter
            .scheme
            .as_deref()
            .is_none_or(|scheme| scheme == "file");
        let pattern = &filter.pattern.glob;
        matches_scheme
            && globset::GlobBuilder::new(pattern)
                .literal_separator(true)
                .build()
                .ok()
                .is_some_and(|glob| glob.compile_matcher().is_match(path))
    })
}

const fn one_of_enabled<T>(capability: &lsp_types::OneOf<bool, T>) -> bool {
    match capability {
        lsp_types::OneOf::Left(enabled) => *enabled,
        lsp_types::OneOf::Right(_) => true,
    }
}

#[derive(Debug)]
struct ResourceSyncProbe {
    path: PathBuf,
    expect_present: bool,
}

#[derive(Debug, PartialEq, Eq)]
enum ResourceDocumentChange {
    Moved(PathBuf),
    Removed,
}

fn resource_document_destination(
    source: &Path,
    operations: &[FileOperation],
) -> Option<ResourceDocumentChange> {
    let mut current = source.to_path_buf();
    let mut changed = false;
    for operation in operations {
        match operation {
            FileOperation::Rename {
                from,
                to,
                overwrite,
            } => {
                if let Ok(relative) = current.strip_prefix(from) {
                    current = to.join(relative);
                    changed = true;
                } else if *overwrite && current.starts_with(to) {
                    return Some(ResourceDocumentChange::Removed);
                }
            }
            FileOperation::Delete { path, .. } if current.starts_with(path) => {
                return Some(ResourceDocumentChange::Removed);
            }
            FileOperation::Create { .. } | FileOperation::Delete { .. } => {}
        }
    }
    changed.then_some(ResourceDocumentChange::Moved(current))
}

fn resource_sync_probes(
    operations: &[FileOperation],
    language: &str,
    extension_map: &std::collections::HashMap<String, String>,
) -> Vec<ResourceSyncProbe> {
    let mut probes = Vec::new();
    for operation in operations {
        match operation {
            FileOperation::Create { path, .. } if path.is_file() => {
                if detect_language(path, extension_map) == language {
                    probes.push(ResourceSyncProbe {
                        path: path.clone(),
                        expect_present: true,
                    });
                }
            }
            FileOperation::Rename { from, to, .. } if to.is_file() => {
                if detect_language(to, extension_map) == language {
                    probes.push(ResourceSyncProbe {
                        path: from.clone(),
                        expect_present: false,
                    });
                    probes.push(ResourceSyncProbe {
                        path: to.clone(),
                        expect_present: true,
                    });
                }
            }
            FileOperation::Rename { from, to, .. } if to.is_dir() => {
                if let Some(destination) = first_provider_file(to, language, extension_map)
                    && let Ok(relative) = destination.strip_prefix(to)
                {
                    probes.push(ResourceSyncProbe {
                        path: from.join(relative),
                        expect_present: false,
                    });
                    probes.push(ResourceSyncProbe {
                        path: destination,
                        expect_present: true,
                    });
                }
            }
            FileOperation::Delete { path, .. }
                if detect_language(path, extension_map) == language =>
            {
                probes.push(ResourceSyncProbe {
                    path: path.clone(),
                    expect_present: false,
                });
            }
            FileOperation::Create { .. }
            | FileOperation::Rename { .. }
            | FileOperation::Delete { .. } => {}
        }
    }
    probes
}

fn first_provider_file(
    root: &Path,
    language: &str,
    extension_map: &std::collections::HashMap<String, String>,
) -> Option<PathBuf> {
    let mut pending = vec![root.to_path_buf()];
    let mut visited = 0usize;
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(directory).ok()?.flatten() {
            visited = visited.saturating_add(1);
            if visited > 4096 {
                return None;
            }
            let file_type = entry.file_type().ok()?;
            let path = entry.path();
            if file_type.is_dir() {
                pending.push(path);
            } else if file_type.is_file() && detect_language(&path, extension_map) == language {
                return Some(path);
            }
        }
    }
    None
}

fn error_indicates_missing_document(error: &Error) -> bool {
    let Error::LspServerError { message, .. } = error else {
        return false;
    };
    let message = message.to_ascii_lowercase();
    message.contains("file not found") || message.contains("document not found")
}

#[cfg(test)]
mod tests {
    use super::{error_indicates_missing_document, plain_text_snippet};
    use crate::Error;

    #[test]
    fn final_cursor_tab_stops_are_safe_to_strip_from_move_item_edits() {
        assert_eq!(
            plain_text_snippet("fn second() {$0}\nfn first() {}"),
            Some("fn second() {}\nfn first() {}".to_string())
        );
        assert_eq!(
            plain_text_snippet("before${0}after"),
            Some("beforeafter".to_string())
        );
    }

    #[test]
    fn interactive_snippet_placeholders_still_fail_closed() {
        assert_eq!(plain_text_snippet(concat!("$", "{1:name}")), None);
        assert_eq!(plain_text_snippet("$1"), None);
        assert_eq!(plain_text_snippet(concat!("$", "{0:default}")), None);
    }

    #[test]
    fn missing_document_errors_prove_deleted_resource_convergence() {
        let missing = Error::LspServerError {
            code: -32603,
            message: "file not found: /workspace/old.rs".to_string(),
            data: None,
        };
        let unrelated = Error::LspServerError {
            code: -32603,
            message: "analysis failed".to_string(),
            data: None,
        };

        assert!(error_indicates_missing_document(&missing));
        assert!(!error_indicates_missing_document(&unrelated));
        assert!(!error_indicates_missing_document(&Error::ServerTerminated));
    }
}
