//! Document symbols and workspace symbol search handlers.

use lsp_types::{
    DocumentSymbol, DocumentSymbolParams, PartialResultParams, TextDocumentIdentifier,
    WorkDoneProgressParams, WorkspaceSymbolParams as LspWorkspaceSymbolParams,
};

use super::Translator;
use super::dto::{
    DocumentSymbolsResult, Symbol, WorkspaceSymbol, WorkspaceSymbolMatch, WorkspaceSymbolMatchMode,
    WorkspaceSymbolOrigin, WorkspaceSymbolResult, WorkspaceSymbolScope,
};
use super::encoding_ctx::EncodingCtx;
use crate::bridge::{ast_grep, lock_std, path_to_uri, uri_to_path};
use crate::config::ToolKind;
use crate::error::{Error, Result};

fn workspace_symbol_match(
    name: &str,
    query: &str,
    mode: WorkspaceSymbolMatchMode,
) -> Option<WorkspaceSymbolMatch> {
    let fuzzy = || {
        let mut name = name.chars();
        query.chars().all(|needle| {
            name.by_ref()
                .any(|candidate| candidate.eq_ignore_ascii_case(&needle))
        })
    };
    let class = if name == query {
        WorkspaceSymbolMatch::Exact
    } else if name.eq_ignore_ascii_case(query) {
        WorkspaceSymbolMatch::ExactCaseInsensitive
    } else if name
        .get(..query.len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(query))
    {
        WorkspaceSymbolMatch::Prefix
    } else if fuzzy() {
        WorkspaceSymbolMatch::Fuzzy
    } else {
        return None;
    };
    match mode {
        WorkspaceSymbolMatchMode::Exact => class <= WorkspaceSymbolMatch::ExactCaseInsensitive,
        WorkspaceSymbolMatchMode::Prefix => class <= WorkspaceSymbolMatch::Prefix,
        WorkspaceSymbolMatchMode::Fuzzy => true,
    }
    .then_some(class)
}

#[cfg(test)]
fn rank_workspace_symbol_names<'a, const N: usize>(
    names: [&'a str; N],
    query: &str,
    mode: WorkspaceSymbolMatchMode,
) -> Vec<(&'a str, WorkspaceSymbolMatch)> {
    let mut matches = names
        .into_iter()
        .filter_map(|name| workspace_symbol_match(name, query, mode).map(|class| (name, class)))
        .collect::<Vec<_>>();
    matches.sort_by_key(|(_, class)| *class);
    matches
}

struct WorkspaceSymbolCandidate {
    name: String,
    kind: String,
    location: lsp_types::Location,
    container_name: Option<String>,
    match_class: WorkspaceSymbolMatch,
    origin: WorkspaceSymbolOrigin,
    project_relative_path: Option<String>,
}

async fn convert_workspace_symbol(
    symbol: WorkspaceSymbolCandidate,
    ctx: &EncodingCtx,
    roots: &[std::path::PathBuf],
    budget: &mut super::source_context::SourceBudget,
) -> WorkspaceSymbol {
    WorkspaceSymbol {
        name: symbol.name,
        kind: symbol.kind,
        location: ctx.location(roots, symbol.location, budget).await,
        container_name: symbol.container_name,
        match_class: symbol.match_class,
        score: symbol.match_class.score(),
        project_relative_path: symbol.project_relative_path,
        origin: symbol.origin,
    }
}

fn workspace_symbol_origin(
    uri: &lsp_types::Uri,
    roots: &[std::path::PathBuf],
) -> (WorkspaceSymbolOrigin, Option<String>) {
    let relative = uri_to_path(uri).and_then(|path| {
        roots
            .iter()
            .find_map(|root| path.strip_prefix(root).ok())
            .map(|path| path.to_string_lossy().into_owned())
    });
    if relative.is_some() {
        (WorkspaceSymbolOrigin::ProjectLocal, relative)
    } else {
        (WorkspaceSymbolOrigin::External, None)
    }
}

async fn finish_workspace_symbols(
    mut candidates: Vec<WorkspaceSymbolCandidate>,
    limit: usize,
    ctx: &EncodingCtx,
    roots: &[std::path::PathBuf],
) -> WorkspaceSymbolResult {
    candidates.sort_by(|left, right| {
        left.match_class
            .cmp(&right.match_class)
            .then_with(|| left.name.cmp(&right.name))
            .then_with(|| left.project_relative_path.cmp(&right.project_relative_path))
            .then_with(|| left.location.uri.as_str().cmp(right.location.uri.as_str()))
            .then_with(|| {
                left.location
                    .range
                    .start
                    .line
                    .cmp(&right.location.range.start.line)
            })
            .then_with(|| {
                left.location
                    .range
                    .start
                    .character
                    .cmp(&right.location.range.start.character)
            })
    });
    let total = candidates.len();
    candidates.truncate(limit);
    let mut budget = super::source_context::SourceBudget::default();
    let mut symbols = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        symbols.push(convert_workspace_symbol(candidate, ctx, roots, &mut budget).await);
    }
    WorkspaceSymbolResult {
        returned: symbols.len(),
        truncated: symbols.len() < total,
        total,
        symbols,
    }
}

/// Validate parameters for `handle_workspace_symbol`.
fn validate_workspace_symbol_params(
    query: &str,
    kind_filter: Option<&str>,
    limit: u32,
) -> Result<()> {
    const MAX_QUERY_LENGTH: usize = 1000;
    const MAX_RESULTS: u32 = 1_000;
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

    if query.len() > MAX_QUERY_LENGTH {
        return Err(Error::InvalidToolParams(format!(
            "Query too long: {} bytes (max {MAX_QUERY_LENGTH})",
            query.len()
        )));
    }
    if limit > MAX_RESULTS {
        return Err(Error::InvalidToolParams(format!(
            "Result limit too large: {limit} (max {MAX_RESULTS})"
        )));
    }

    if let Some(kind) = kind_filter
        && !VALID_SYMBOL_KINDS
            .iter()
            .any(|k| k.eq_ignore_ascii_case(kind))
    {
        return Err(Error::InvalidToolParams(format!(
            "Invalid kind_filter: '{kind}'. Valid values: {VALID_SYMBOL_KINDS:?}"
        )));
    }

    Ok(())
}

/// Convert LSP document symbol to MCP symbol. `uri` is the queried
/// document's own URI: nested `DocumentSymbol` entries have no URI of their
/// own, since `textDocument/documentSymbol` is always scoped to one file.
///
/// Boxed because it recurses through `children` and an `async fn` cannot
/// call itself directly (its future would have unbounded size).
fn convert_document_symbol<'a>(
    symbol: DocumentSymbol,
    ctx: &'a EncodingCtx,
    uri: &'a lsp_types::Uri,
) -> futures::future::BoxFuture<'a, Symbol> {
    Box::pin(async move {
        let range = ctx.normalize_range(uri, symbol.range).await;
        let selection_range = ctx.normalize_range(uri, symbol.selection_range).await;
        let children = match symbol.children {
            Some(children) => {
                let mut result = Vec::with_capacity(children.len());
                for child in children {
                    result.push(convert_document_symbol(child, ctx, uri).await);
                }
                Some(result)
            }
            None => None,
        };

        Symbol {
            name: symbol.name,
            kind: format!("{:?}", symbol.kind),
            range,
            selection_range,
            symbol_handle: None,
            children,
        }
    })
}

impl Translator {
    /// Handle document symbols request.
    ///
    /// # Errors
    ///
    /// Returns an error if the LSP request fails, the file cannot be opened,
    /// or the routed server does not advertise `documentSymbolProvider` support.
    pub async fn handle_document_symbols(
        &self,
        file_path: String,
    ) -> Result<DocumentSymbolsResult> {
        let (server_id, client, uri) = self
            .prepare_gated_document(
                &file_path,
                ToolKind::DocumentSymbols,
                "documentSymbolProvider",
                |caps| {
                    matches!(
                        caps.document_symbol_provider,
                        Some(lsp_types::OneOf::Left(true) | lsp_types::OneOf::Right(_))
                    )
                },
            )
            .await?;
        let ctx = self.encoding_ctx(&server_id);
        let response_uri = uri.clone();

        let params = DocumentSymbolParams {
            text_document: TextDocumentIdentifier { uri },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };

        let response: Option<lsp_types::DocumentSymbolResponse> = client
            .request(
                "textDocument/documentSymbol",
                params,
                client.request_timeout(),
            )
            .await?;

        let symbols = match response {
            Some(lsp_types::DocumentSymbolResponse::Flat(symbols)) => {
                let mut result = Vec::with_capacity(symbols.len());
                for sym in symbols {
                    let range = ctx
                        .normalize_range(&sym.location.uri, sym.location.range)
                        .await;
                    let selection_range = ctx
                        .normalize_range(&sym.location.uri, sym.location.range)
                        .await;
                    result.push(Symbol {
                        name: sym.name,
                        kind: format!("{:?}", sym.kind),
                        range,
                        selection_range,
                        symbol_handle: None,
                        children: None,
                    });
                }
                result
            }
            Some(lsp_types::DocumentSymbolResponse::Nested(symbols)) => {
                let mut result = Vec::with_capacity(symbols.len());
                for sym in symbols {
                    result.push(convert_document_symbol(sym, &ctx, &response_uri).await);
                }
                result
            }
            None => vec![],
        };

        Ok(DocumentSymbolsResult { symbols })
    }

    /// Handle workspace symbol search.
    ///
    /// # Errors
    ///
    /// Returns an error when the request parameters are invalid. Missing,
    /// initializing, unsupported, or failed LSP servers use the bounded
    /// in-process AST fallback.
    #[allow(clippy::too_many_lines)]
    pub async fn handle_workspace_symbol(
        &self,
        query: String,
        kind_filter: Option<String>,
        limit: u32,
        match_mode: WorkspaceSymbolMatchMode,
        scope: WorkspaceSymbolScope,
    ) -> Result<WorkspaceSymbolResult> {
        validate_workspace_symbol_params(&query, kind_filter.as_deref(), limit)?;
        if limit == 0 {
            return Ok(WorkspaceSymbolResult {
                symbols: Vec::new(),
                total: 0,
                returned: 0,
                truncated: false,
            });
        }

        let fallback = || async {
            let mut languages = lock_std(&self.project_lsp_configs)
                .iter()
                .map(|config| config.language_id.clone())
                .chain(self.extension_map.values().cloned())
                .collect::<Vec<_>>();
            languages.sort_unstable();
            languages.dedup();
            Ok(self
                .ast_grep_workspace_symbols(
                    &languages,
                    &query,
                    kind_filter.as_deref(),
                    limit as usize,
                    match_mode,
                )
                .await)
        };

        // Workspace search has no document, so it resolves via `resolve_any`
        // rather than a per-language route. If the resolved server is not
        // registered yet but is expected, tell the caller to wait and retry
        // rather than implying nothing is configured.
        let routed = {
            lock_std(&self.router)
                .resolve_any(ToolKind::WorkspaceSymbols)
                .cloned()
        };
        let server_id = match routed {
            Ok(server_id) => server_id,
            Err(reason) => {
                tracing::debug!(?reason, "using AST workspace-symbol fallback");
                return fallback().await;
            }
        };
        if lock_std(&self.expected_servers).contains(&server_id) {
            tracing::debug!(%server_id, "workspace-symbol server still initializing; using AST fallback");
            return fallback().await;
        }
        if let Err(error) = self.respawn_if_dead(&server_id).await {
            tracing::debug!(%error, "workspace-symbol server unavailable; using AST fallback");
            return fallback().await;
        }
        let client = lock_std(&self.lsp_clients).get(&server_id).cloned();
        let Some(client) = client else {
            return fallback().await;
        };
        if self
            .require_capability(&server_id, "workspaceSymbolProvider", |caps| {
                matches!(
                    caps.workspace_symbol_provider,
                    Some(lsp_types::OneOf::Left(true) | lsp_types::OneOf::Right(_))
                )
            })
            .is_err()
        {
            return fallback().await;
        }

        let params = LspWorkspaceSymbolParams {
            query: query.clone(),
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };

        let response: Option<Vec<lsp_types::SymbolInformation>> = match client
            .request("workspace/symbol", params, client.request_timeout())
            .await
        {
            Ok(response) => response,
            Err(error) => {
                tracing::debug!(%error, "workspace-symbol request failed; using AST fallback");
                return fallback().await;
            }
        };

        let ctx = self.encoding_ctx(&server_id);
        let mut candidates = Vec::new();
        for sym in response.unwrap_or_default() {
            let Some(match_class) = workspace_symbol_match(&sym.name, &query, match_mode) else {
                continue;
            };
            let (origin, project_relative_path) =
                workspace_symbol_origin(&sym.location.uri, &self.workspace_roots);
            if scope == WorkspaceSymbolScope::Project && origin == WorkspaceSymbolOrigin::External {
                continue;
            }
            let kind = format!("{:?}", sym.kind);
            if kind_filter
                .as_deref()
                .is_some_and(|filter| !kind.eq_ignore_ascii_case(filter))
            {
                continue;
            }
            candidates.push(WorkspaceSymbolCandidate {
                name: sym.name,
                kind,
                location: sym.location,
                container_name: sym.container_name,
                match_class,
                origin,
                project_relative_path,
            });
        }

        Ok(finish_workspace_symbols(candidates, limit as usize, &ctx, &self.workspace_roots).await)
    }

    async fn ast_grep_workspace_symbols(
        &self,
        languages: &[String],
        query: &str,
        kind_filter: Option<&str>,
        limit: usize,
        match_mode: WorkspaceSymbolMatchMode,
    ) -> WorkspaceSymbolResult {
        let candidate_query = query
            .chars()
            .next()
            .map(|character| character.to_string())
            .unwrap_or_default();
        let matches = ast_grep::search(
            &self.workspace_roots,
            languages,
            &candidate_query,
            kind_filter,
            4_096,
        )
        .await;
        let ctx = EncodingCtx {
            encoding: crate::bridge::encoding::PositionEncoding::Utf8,
            tracker: self.document_tracker.clone(),
        };
        let mut candidates = Vec::new();
        for symbol in matches {
            let Some(kind) = ast_grep_symbol_kind(&symbol.kind) else {
                continue;
            };
            if kind_filter.is_some_and(|filter| !kind.eq_ignore_ascii_case(filter)) {
                continue;
            }
            let Some(match_class) = workspace_symbol_match(&symbol.name, query, match_mode) else {
                continue;
            };
            let Ok(uri) = path_to_uri(&symbol.path) else {
                continue;
            };
            let range = lsp_types::Range {
                start: lsp_types::Position::new(symbol.start_line, symbol.start_character),
                end: lsp_types::Position::new(symbol.end_line, symbol.end_character),
            };
            let location = lsp_types::Location { uri, range };
            let (origin, project_relative_path) =
                workspace_symbol_origin(&location.uri, &self.workspace_roots);
            candidates.push(WorkspaceSymbolCandidate {
                name: symbol.name,
                kind: kind.to_string(),
                location,
                container_name: None,
                match_class,
                project_relative_path,
                origin,
            });
        }
        finish_workspace_symbols(candidates, limit, &ctx, &self.workspace_roots).await
    }
}

fn ast_grep_symbol_kind(symbol_type: &str) -> Option<&'static str> {
    match symbol_type.to_ascii_lowercase().as_str() {
        "class" => Some("Class"),
        "constant" | "const" => Some("Constant"),
        "enum" => Some("Enum"),
        "enum_member" => Some("EnumMember"),
        "field" => Some("Field"),
        "function" => Some("Function"),
        "interface" | "trait" => Some("Interface"),
        "method" => Some("Method"),
        "module" | "namespace" => Some("Module"),
        "struct" | "union" => Some("Struct"),
        "type" | "typealias" => Some("TypeParameter"),
        "variable" => Some("Variable"),
        _ => None,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::collections::{HashMap, HashSet};

    #[test]
    fn workspace_symbol_ranking_is_exact_first_and_stable() {
        let ranked = rank_workspace_symbol_names(
            ["target_extra", "Target", "other_target", "target", "target"],
            "target",
            WorkspaceSymbolMatchMode::Fuzzy,
        );

        assert_eq!(
            ranked,
            [
                ("target", WorkspaceSymbolMatch::Exact),
                ("target", WorkspaceSymbolMatch::Exact),
                ("Target", WorkspaceSymbolMatch::ExactCaseInsensitive),
                ("target_extra", WorkspaceSymbolMatch::Prefix),
                ("other_target", WorkspaceSymbolMatch::Fuzzy),
            ]
        );
    }

    #[test]
    fn workspace_symbol_match_modes_have_explicit_boundaries() {
        let names = ["target", "Target", "target_extra", "other_target"];
        assert_eq!(
            rank_workspace_symbol_names(names, "target", WorkspaceSymbolMatchMode::Exact).len(),
            2
        );
        assert_eq!(
            rank_workspace_symbol_names(names, "target", WorkspaceSymbolMatchMode::Prefix).len(),
            3
        );
        assert_eq!(
            rank_workspace_symbol_names(names, "target", WorkspaceSymbolMatchMode::Fuzzy).len(),
            4
        );
        assert_eq!(WorkspaceSymbolMatch::Exact.score(), 100);
    }
    use std::fs;
    use std::time::Duration;

    use super::*;
    use crate::bridge::translator::testing::{
        read_framed_message, translator_with_capabilities, write_response,
    };
    use crate::config::{ServerId, ToolRouter};
    use tempfile::TempDir;
    use tokio::io::BufReader;
    use tokio::time::timeout;

    fn fallback_translator(dir: &TempDir) -> Translator {
        let mut translator = Translator::new()
            .with_extensions(HashMap::from([("rs".to_string(), "rust".to_string())]));
        translator.set_workspace_roots(vec![dir.path().to_path_buf()]);
        fs::write(dir.path().join("main.rs"), "fn fallback_target() {}\n").unwrap();
        translator
    }

    #[tokio::test]
    async fn test_handle_workspace_symbol_no_server() {
        let dir = TempDir::new().unwrap();
        let translator = fallback_translator(&dir);
        let result = translator
            .handle_workspace_symbol(
                "fallback_target".to_string(),
                None,
                100,
                WorkspaceSymbolMatchMode::default(),
                WorkspaceSymbolScope::default(),
            )
            .await
            .unwrap();
        assert_eq!(result.symbols.len(), 1);
        assert_eq!(result.symbols[0].name, "fallback_target");
    }

    #[tokio::test]
    async fn fallback_workspace_symbols_are_ranked_source_bearing_and_bounded() {
        let dir = TempDir::new().unwrap();
        let unicode = dir.path().join("ünicode");
        fs::create_dir(&unicode).unwrap();
        fs::write(
            dir.path().join("a.rs"),
            "fn target() {}\nfn target_extra() {}\n",
        )
        .unwrap();
        fs::write(
            unicode.join("b.rs"),
            "fn target() {}\nfn Target() {}\nfn other_target() {}\n",
        )
        .unwrap();
        let mut translator = Translator::new()
            .with_extensions(HashMap::from([("rs".to_owned(), "rust".to_owned())]));
        translator.set_workspace_roots(vec![dir.path().to_path_buf()]);

        let result = translator
            .handle_workspace_symbol(
                "target".to_owned(),
                Some("Function".to_owned()),
                2,
                WorkspaceSymbolMatchMode::Fuzzy,
                WorkspaceSymbolScope::Project,
            )
            .await
            .unwrap();

        assert_eq!(
            (result.total, result.returned, result.truncated),
            (5, 2, true)
        );
        assert!(
            result
                .symbols
                .iter()
                .all(|symbol| symbol.match_class == WorkspaceSymbolMatch::Exact)
        );
        assert_eq!(
            result.symbols[0].project_relative_path.as_deref(),
            Some("a.rs")
        );
        assert!(matches!(
            result.symbols[0].location.source,
            crate::bridge::SourceContext::Available(_)
        ));
        assert!(result.symbols[0].location.symbol_handle.is_none());
    }

    #[tokio::test]
    async fn lsp_workspace_symbols_rank_before_source_budget_and_require_external_opt_in() {
        let dir = TempDir::new().unwrap();
        let exact_path = dir.path().join("exact.rs");
        fs::write(&exact_path, "fn target() {}\n").unwrap();
        let external = TempDir::new().unwrap();
        let external_path = external.path().join("external.rs");
        fs::write(&external_path, "fn target() {}\n").unwrap();
        let server_id = ServerId::from("rust");
        let capabilities = lsp_types::ServerCapabilities {
            workspace_symbol_provider: Some(lsp_types::OneOf::Left(true)),
            ..lsp_types::ServerCapabilities::default()
        };
        let (translator, mut server) = translator_with_capabilities(&dir, &server_id, capabilities);

        let mut response = Vec::new();
        for index in 0..9 {
            let path = dir.path().join(format!("fuzzy-{index}.rs"));
            fs::write(&path, format!("fn t{index}arget() {{}}\n")).unwrap();
            response.push(serde_json::json!({
                "name": format!("t{index}arget"),
                "kind": 12,
                "location": {
                    "uri": path_to_uri(&path).unwrap().to_string(),
                    "range": { "start": { "line": 0, "character": 3 }, "end": { "line": 0, "character": 11 } }
                }
            }));
        }
        response.push(serde_json::json!({
            "name": "target",
            "kind": 12,
            "location": {
                "uri": path_to_uri(&exact_path).unwrap().to_string(),
                "range": { "start": { "line": 0, "character": 3 }, "end": { "line": 0, "character": 9 } }
            }
        }));
        response.push(serde_json::json!({
            "name": "target",
            "kind": 12,
            "location": {
                "uri": path_to_uri(&external_path).unwrap().to_string(),
                "range": { "start": { "line": 0, "character": 3 }, "end": { "line": 0, "character": 9 } }
            }
        }));

        let responder = tokio::spawn(async move {
            let mut wire = BufReader::new(&mut server.write_stdout);
            for _ in 0..2 {
                let request = read_framed_message(&mut wire).await;
                write_response(
                    &mut server.read_half_stdin,
                    &request["id"],
                    serde_json::Value::Array(response.clone()),
                )
                .await;
            }
        });

        let project = translator
            .handle_workspace_symbol(
                "target".to_owned(),
                None,
                1,
                WorkspaceSymbolMatchMode::Fuzzy,
                WorkspaceSymbolScope::Project,
            )
            .await
            .unwrap();
        assert_eq!((project.total, project.returned), (10, 1));
        assert_eq!(project.symbols[0].name, "target");
        assert!(matches!(
            project.symbols[0].location.source,
            crate::bridge::SourceContext::Available(_)
        ));

        let all = translator
            .handle_workspace_symbol(
                "target".to_owned(),
                None,
                20,
                WorkspaceSymbolMatchMode::Exact,
                WorkspaceSymbolScope::All,
            )
            .await
            .unwrap();
        responder.await.unwrap();
        assert_eq!(all.total, 2);
        assert!(
            all.symbols
                .iter()
                .any(|symbol| symbol.origin == WorkspaceSymbolOrigin::External)
        );
    }

    /// #242/S4 regression: a server is configured and still spawning (large
    /// project load) rather than never having existed -- the router alone
    /// cannot tell these apart (both look like "nothing registered"), so
    /// `handle_workspace_symbol` must remain useful while the configured
    /// server is still initializing.
    #[tokio::test]
    async fn test_handle_workspace_symbol_reports_initializing_when_expected_but_not_registered() {
        let dir = TempDir::new().unwrap();
        let translator = fallback_translator(&dir);
        translator.set_expected_servers(HashSet::from([ServerId::from("pyright")]));

        let result = translator
            .handle_workspace_symbol(
                "fallback_target".to_string(),
                None,
                100,
                WorkspaceSymbolMatchMode::default(),
                WorkspaceSymbolScope::default(),
            )
            .await
            .unwrap();
        assert_eq!(result.symbols[0].name, "fallback_target");
    }

    /// MCPLS-43 regression: rust-analyzer accepts requests before its first
    /// indexing pass is authoritative. An empty response from that interval
    /// must not masquerade as a successful semantic result.
    #[tokio::test]
    async fn test_handle_workspace_symbol_falls_back_while_registered_server_initializes() {
        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from("rust");
        let capabilities = lsp_types::ServerCapabilities {
            workspace_symbol_provider: Some(lsp_types::OneOf::Left(true)),
            ..lsp_types::ServerCapabilities::default()
        };
        let (translator, mut server) = translator_with_capabilities(&dir, &server_id, capabilities);
        translator.set_expected_servers(HashSet::from([server_id]));
        fs::write(dir.path().join("main.rs"), "fn fallback_target() {}\n").unwrap();

        let responder = tokio::spawn(async move {
            let mut wire = BufReader::new(&mut server.write_stdout);
            let request = read_framed_message(&mut wire).await;
            assert_eq!(request["method"], "workspace/symbol");
            write_response(
                &mut server.read_half_stdin,
                &request["id"],
                serde_json::json!([]),
            )
            .await;
        });

        let result = timeout(
            Duration::from_secs(2),
            translator.handle_workspace_symbol(
                "fallback_target".to_string(),
                None,
                100,
                WorkspaceSymbolMatchMode::default(),
                WorkspaceSymbolScope::default(),
            ),
        )
        .await
        .expect("handler call should not hang")
        .unwrap();
        responder.abort();
        assert_eq!(result.symbols.len(), 1);
        assert_eq!(result.symbols[0].name, "fallback_target");
    }

    /// #242 regression: a server *is* configured and running, it just
    /// doesn't claim `workspace_symbols` and there is no catch-all -- the
    /// structural fallback must still answer the query.
    #[tokio::test]
    async fn test_handle_workspace_symbol_no_claimant_names_tool() {
        let configs = vec![crate::config::LspServerConfig {
            language_id: "python".to_string(),
            command: "pyright-langserver".to_string(),
            args: vec![],
            env: HashMap::new(),
            file_patterns: vec![],
            initialization_options: None,
            timeout_seconds: 30,
            request_timeout_seconds: 30,
            heuristics: None,
            name: Some("pyright".to_string()),
            handles: Some(vec![ToolKind::Hover]),
        }];
        let router = ToolRouter::from_configs(&configs).unwrap();
        let dir = TempDir::new().unwrap();
        let mut translator = fallback_translator(&dir).with_router(router);
        translator.set_workspace_roots(vec![dir.path().to_path_buf()]);

        let result = translator
            .handle_workspace_symbol(
                "fallback_target".to_string(),
                None,
                100,
                WorkspaceSymbolMatchMode::default(),
                WorkspaceSymbolScope::default(),
            )
            .await
            .unwrap();
        assert_eq!(result.symbols[0].name, "fallback_target");
    }
}
