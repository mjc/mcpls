//! Hover, go-to-definition/implementation/type-definition, and references
//! handlers.

use lsp_types::{
    GotoDefinitionParams, Hover, HoverContents, HoverParams as LspHoverParams, MarkedString,
    PartialResultParams, ReferenceContext, ReferenceParams, TextDocumentIdentifier,
    TextDocumentPositionParams, WorkDoneProgressParams,
};

use super::Translator;
use super::dto::{
    DefinitionResult, DocumentSymbolOptions, HoverResult, Location, LocationsResult,
    NavigationKind, Position2D, Range, ReferenceGroup, ReferenceRole, ReferenceUse,
    ReferencesResult, SemanticResultLimits, Symbol,
};
use super::encoding_ctx::EncodingCtx;
use super::source_context::SourceBudget;
use crate::config::ToolKind;
use crate::error::Result;

const MAX_NAVIGATION_TARGETS: usize = 64;

/// Normalize a `GotoDefinitionResponse` into bounded MCP `Location` values.
pub(super) async fn bounded_locations(
    response: Option<lsp_types::GotoDefinitionResponse>,
    ctx: &EncodingCtx,
    workspace_roots: &[std::path::PathBuf],
    max_targets: usize,
) -> (Vec<Location>, bool) {
    let (lsp_locs, truncated_items) = bounded_targets(flatten_goto_response(response), max_targets);
    let mut locations = Vec::with_capacity(lsp_locs.len());
    let mut budget = SourceBudget::default();
    for loc in lsp_locs {
        locations.push(ctx.location(workspace_roots, loc, &mut budget).await);
    }
    (locations, truncated_items || budget.truncated())
}

fn bounded_targets(
    mut locations: Vec<lsp_types::Location>,
    max_targets: usize,
) -> (Vec<lsp_types::Location>, bool) {
    let truncated = locations.len() > max_targets;
    locations.truncate(max_targets);
    (locations, truncated)
}

fn flatten_goto_response(
    response: Option<lsp_types::GotoDefinitionResponse>,
) -> Vec<lsp_types::Location> {
    match response {
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
    }
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

impl Translator {
    /// Handle hover request.
    ///
    /// # Errors
    ///
    /// Returns an error if the LSP request fails, the file cannot be opened,
    /// or the routed server does not advertise `hoverProvider` support.
    pub async fn handle_hover(
        &self,
        file_path: String,
        line: u32,
        character: u32,
    ) -> Result<HoverResult> {
        let (server_id, client, uri) = self
            .prepare_gated_document(&file_path, ToolKind::Hover, "hoverProvider", |caps| {
                matches!(
                    caps.hover_provider,
                    Some(
                        lsp_types::HoverProviderCapability::Simple(true)
                            | lsp_types::HoverProviderCapability::Options(_)
                    )
                )
            })
            .await?;
        let ctx = self.encoding_ctx(&server_id);
        let lsp_position = ctx.to_lsp(&uri, line, character).await;
        let response_uri = uri.clone();

        let params = LspHoverParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
                position: lsp_position,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
        };

        let response: Option<Hover> = client
            .request("textDocument/hover", params, client.request_timeout())
            .await?;

        let (contents, range) = match response {
            Some(hover) => {
                let contents = extract_hover_contents(hover.contents);
                let range = match hover.range {
                    Some(r) => Some(ctx.normalize_range(&response_uri, r).await),
                    None => None,
                };
                (contents, range)
            }
            None => ("No hover information available".to_string(), None),
        };
        let source_range = range.clone().unwrap_or(Range {
            start: Position2D { line, character },
            end: Position2D { line, character },
        });
        let mut budget = SourceBudget::default();
        let source = ctx
            .source_context(
                &self.workspace_roots,
                &response_uri,
                source_range,
                &mut budget,
            )
            .await;
        let result = HoverResult {
            provider: "standard_lsp".to_owned(),
            kind: NavigationKind::Hover,
            contents,
            range,
            source,
            truncated: budget.truncated(),
            symbol_handle: None,
        };

        Ok(result)
    }

    /// Handle definition request.
    ///
    /// # Errors
    ///
    /// Returns an error if the LSP request fails, the file cannot be opened,
    /// or the routed server does not advertise `definitionProvider` support.
    pub async fn handle_definition(
        &self,
        file_path: String,
        line: u32,
        character: u32,
    ) -> Result<DefinitionResult> {
        let (server_id, client, uri) = self
            .prepare_gated_document(
                &file_path,
                ToolKind::Definition,
                "definitionProvider",
                |caps| {
                    matches!(
                        caps.definition_provider,
                        Some(lsp_types::OneOf::Left(true) | lsp_types::OneOf::Right(_))
                    )
                },
            )
            .await?;
        let ctx = self.encoding_ctx(&server_id);
        let lsp_position = ctx.to_lsp(&uri, line, character).await;

        let params = GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
                position: lsp_position,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };

        let response: Option<lsp_types::GotoDefinitionResponse> = client
            .request("textDocument/definition", params, client.request_timeout())
            .await?;

        let (locations, truncated) = bounded_locations(
            response,
            &ctx,
            &self.workspace_roots,
            MAX_NAVIGATION_TARGETS,
        )
        .await;
        let result = DefinitionResult {
            provider: "standard_lsp".to_owned(),
            kind: NavigationKind::Definition,
            locations,
            truncated,
        };

        Ok(result)
    }

    /// Handle references request.
    ///
    /// # Errors
    ///
    /// Returns an error if the LSP request fails, the file cannot be opened,
    /// or the routed server does not advertise `referencesProvider` support.
    pub async fn handle_references(
        &self,
        file_path: String,
        line: u32,
        character: u32,
        include_declaration: bool,
        limits: SemanticResultLimits,
    ) -> Result<ReferencesResult> {
        let (server_id, client, uri) = self
            .prepare_gated_document(
                &file_path,
                ToolKind::References,
                "referencesProvider",
                |caps| {
                    matches!(
                        caps.references_provider,
                        Some(lsp_types::OneOf::Left(true) | lsp_types::OneOf::Right(_))
                    )
                },
            )
            .await?;
        let ctx = self.encoding_ctx(&server_id);
        let lsp_position = ctx.to_lsp(&uri, line, character).await;

        let params = ReferenceParams {
            text_document_position: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                position: lsp_position,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
            context: ReferenceContext {
                include_declaration,
            },
        };

        let response: Option<Vec<lsp_types::Location>> = client
            .request("textDocument/references", params, client.request_timeout())
            .await?;

        let mut locations = response.unwrap_or_default();
        let declaration = if include_declaration {
            let definition_params = GotoDefinitionParams {
                text_document_position_params: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier { uri: uri.clone() },
                    position: lsp_position,
                },
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
            };
            let definitions: Option<lsp_types::GotoDefinitionResponse> = client
                .request(
                    "textDocument/definition",
                    definition_params,
                    client.request_timeout(),
                )
                .await
                .unwrap_or_default();
            let declaration = flatten_goto_response(definitions).into_iter().next();
            if let Some(index) = declaration.as_ref().and_then(|declaration| {
                locations
                    .iter()
                    .position(|location| location == declaration)
            }) {
                locations.remove(index);
            }
            declaration
        } else {
            None
        };

        let total_references = locations.len() + usize::from(declaration.is_some());
        let mut result_locations = Vec::with_capacity(locations.len());
        let mut budget = SourceBudget::default();
        let declaration = if let Some(location) = declaration {
            Some(ReferenceUse {
                location: ctx
                    .location(&self.workspace_roots, location, &mut budget)
                    .await,
                role: ReferenceRole::Declaration,
            })
        } else {
            None
        };
        for loc in locations {
            result_locations.push(ctx.location(&self.workspace_roots, loc, &mut budget).await);
        }
        let enclosing_symbols = self.reference_symbols(&result_locations).await;
        let result = group_references(
            result_locations,
            declaration,
            total_references,
            &self.workspace_roots,
            &enclosing_symbols,
            limits,
            budget.truncated(),
        );

        Ok(result)
    }

    /// Handle go-to-implementation request (`textDocument/implementation`).
    ///
    /// Returns the locations of trait method or interface member implementations.
    ///
    /// # Errors
    ///
    /// Returns an error if the LSP request fails, the file cannot be opened,
    /// or the routed server does not advertise `implementationProvider` support.
    pub async fn handle_implementation(
        &self,
        file_path: String,
        line: u32,
        character: u32,
    ) -> Result<LocationsResult> {
        let (server_id, client, uri) = self
            .prepare_gated_document(
                &file_path,
                ToolKind::Implementation,
                "implementationProvider",
                |caps| {
                    matches!(
                        caps.implementation_provider,
                        Some(
                            lsp_types::ImplementationProviderCapability::Simple(true)
                                | lsp_types::ImplementationProviderCapability::Options(_)
                        )
                    )
                },
            )
            .await?;
        let ctx = self.encoding_ctx(&server_id);
        let lsp_position = ctx.to_lsp(&uri, line, character).await;

        let params = GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
                position: lsp_position,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };

        let response: Option<lsp_types::GotoDefinitionResponse> = client
            .request(
                "textDocument/implementation",
                params,
                client.request_timeout(),
            )
            .await?;

        let (locations, truncated) = bounded_locations(
            response,
            &ctx,
            &self.workspace_roots,
            MAX_NAVIGATION_TARGETS,
        )
        .await;
        Ok(LocationsResult {
            provider: "standard_lsp".to_owned(),
            kind: NavigationKind::Implementation,
            locations,
            truncated,
        })
    }

    /// Handle go-to-type-definition request (`textDocument/typeDefinition`).
    ///
    /// Returns the type definition location of the expression at position. Distinct
    /// from go-to-definition for variable bindings where definition and type differ.
    ///
    /// # Errors
    ///
    /// Returns an error if the LSP request fails, the file cannot be opened,
    /// or the routed server does not advertise `typeDefinitionProvider` support.
    pub async fn handle_type_definition(
        &self,
        file_path: String,
        line: u32,
        character: u32,
    ) -> Result<LocationsResult> {
        let (server_id, client, uri) = self
            .prepare_gated_document(
                &file_path,
                ToolKind::TypeDefinition,
                "typeDefinitionProvider",
                |caps| {
                    matches!(
                        caps.type_definition_provider,
                        Some(
                            lsp_types::TypeDefinitionProviderCapability::Simple(true)
                                | lsp_types::TypeDefinitionProviderCapability::Options(_)
                        )
                    )
                },
            )
            .await?;
        let ctx = self.encoding_ctx(&server_id);
        let lsp_position = ctx.to_lsp(&uri, line, character).await;

        let params = GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
                position: lsp_position,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };

        let response: Option<lsp_types::GotoDefinitionResponse> = client
            .request(
                "textDocument/typeDefinition",
                params,
                client.request_timeout(),
            )
            .await?;

        let (locations, truncated) = bounded_locations(
            response,
            &ctx,
            &self.workspace_roots,
            MAX_NAVIGATION_TARGETS,
        )
        .await;
        Ok(LocationsResult {
            provider: "standard_lsp".to_owned(),
            kind: NavigationKind::TypeDefinition,
            locations,
            truncated,
        })
    }
}

impl Translator {
    async fn reference_symbols(
        &self,
        locations: &[Location],
    ) -> std::collections::HashMap<String, Vec<Symbol>> {
        let paths = locations
            .iter()
            .filter_map(|location| location.path.as_ref())
            .filter(|path| {
                self.workspace_roots
                    .iter()
                    .any(|root| std::path::Path::new(path.as_str()).starts_with(root))
            })
            .cloned()
            .collect::<std::collections::BTreeSet<_>>();
        let requests = paths.into_iter().map(|path| async move {
            let result = self
                .handle_document_symbols(path.clone(), DocumentSymbolOptions::internal_tree())
                .await;
            (path, result)
        });
        futures::future::join_all(requests)
            .await
            .into_iter()
            .filter_map(|(path, result)| result.ok().map(|result| (path, result.symbols)))
            .collect()
    }
}

fn group_references(
    mut locations: Vec<Location>,
    declaration: Option<ReferenceUse>,
    total_references: usize,
    workspace_roots: &[std::path::PathBuf],
    enclosing_symbols: &std::collections::HashMap<String, Vec<Symbol>>,
    limits: SemanticResultLimits,
    source_truncated: bool,
) -> ReferencesResult {
    locations.sort_by(|left, right| {
        reference_source_order(left, workspace_roots)
            .cmp(&reference_source_order(right, workspace_roots))
            .then_with(|| left.path.cmp(&right.path))
            .then_with(|| left.range.start.line.cmp(&right.range.start.line))
            .then_with(|| left.range.start.character.cmp(&right.range.start.character))
    });
    let mut groups: Vec<ReferenceGroup> = Vec::new();
    for location in locations {
        let path = location.path.as_deref().unwrap_or(&location.uri);
        let project_relative_path = workspace_roots
            .iter()
            .find_map(|root| std::path::Path::new(path).strip_prefix(root).ok())
            .map_or_else(
                || path.to_owned(),
                |path| path.to_string_lossy().into_owned(),
            );
        let enclosing_symbol = location
            .path
            .as_ref()
            .and_then(|path| enclosing_symbols.get(path))
            .and_then(|symbols| smallest_enclosing_symbol(symbols, &location.range.start));
        let reference = ReferenceUse {
            location,
            role: ReferenceRole::Unknown,
        };
        if let Some(group) = groups.last_mut().filter(|group| {
            group.project_relative_path == project_relative_path
                && group.enclosing_symbol == enclosing_symbol
        }) {
            group.references.push(reference);
        } else {
            groups.push(ReferenceGroup {
                project_relative_path,
                enclosing_symbol,
                references: vec![reference],
            });
        }
    }
    let total_groups = groups.len();
    let declaration = declaration.filter(|_| limits.total > 0);
    let mut returned_references = usize::from(declaration.is_some());
    let mut per_file = std::collections::HashMap::<String, usize>::new();
    groups.retain_mut(|group| {
        let file_count = per_file
            .entry(group.project_relative_path.clone())
            .or_default();
        if *file_count >= limits.per_file || returned_references >= limits.total {
            return false;
        }
        let available = limits
            .total
            .saturating_sub(returned_references)
            .min(limits.per_file.saturating_sub(*file_count))
            .min(limits.per_symbol);
        group.references.truncate(available);
        *file_count += group.references.len();
        returned_references += group.references.len();
        !group.references.is_empty()
    });
    let returned_groups = groups.len();
    let omitted_groups = total_groups.saturating_sub(returned_groups);
    ReferencesResult {
        provider: "standard_lsp".to_owned(),
        groups,
        declaration,
        total_references,
        returned_references,
        total_groups,
        returned_groups,
        omitted_groups,
        limits,
        truncated: source_truncated || returned_references < total_references,
    }
}

fn reference_source_order(location: &Location, roots: &[std::path::PathBuf]) -> u8 {
    let Some(path) = location.path.as_deref().map(std::path::Path::new) else {
        return 2;
    };
    if !roots.iter().any(|root| path.starts_with(root)) {
        return 2;
    }
    let text = path.to_string_lossy();
    u8::from(text.contains("/tests/") || text.ends_with("_test.rs"))
}

fn smallest_enclosing_symbol(symbols: &[Symbol], position: &Position2D) -> Option<String> {
    smallest_enclosing_symbol_at(symbols, (position.line, position.character))
}

fn smallest_enclosing_symbol_at(symbols: &[Symbol], position: (u32, u32)) -> Option<String> {
    symbols.iter().find_map(|symbol| {
        let start = (symbol.range.start.line, symbol.range.start.character);
        let end = (symbol.range.end.line, symbol.range.end.character);
        if position < start || position > end {
            return None;
        }
        symbol
            .children
            .as_deref()
            .and_then(|children| smallest_enclosing_symbol_at(children, position))
            .or_else(|| Some(symbol.name.clone()))
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn unavailable_location(path: &str, line: u32) -> Location {
        Location {
            path: Some(path.to_owned()),
            uri: format!("file://{path}"),
            range: Range {
                start: Position2D { line, character: 1 },
                end: Position2D { line, character: 2 },
            },
            source: super::super::dto::SourceContext::Unavailable {
                reason: super::super::dto::SourceUnavailableReason::NotFound,
            },
            symbol_handle: None,
        }
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

    #[test]
    fn navigation_target_limit_is_explicit_for_multiple_definitions() {
        let uri: lsp_types::Uri = "file:///workspace/lib.rs".parse().unwrap();
        let location = lsp_types::Location {
            uri,
            range: lsp_types::Range::default(),
        };
        let locations =
            flatten_goto_response(Some(lsp_types::GotoDefinitionResponse::Array(vec![
                location;
                MAX_NAVIGATION_TARGETS
                    + 1
            ])));
        let (locations, truncated) = bounded_targets(locations, MAX_NAVIGATION_TARGETS);

        assert_eq!(locations.len(), MAX_NAVIGATION_TARGETS);
        assert!(truncated);
    }

    #[test]
    fn references_serialize_grouped_file_counts() {
        let result = group_references(
            vec![
                unavailable_location("/workspace/src/lib.rs", 2),
                unavailable_location("/workspace/src/lib.rs", 4),
            ],
            None,
            2,
            &[std::path::PathBuf::from("/workspace")],
            &std::collections::HashMap::new(),
            SemanticResultLimits::default(),
            false,
        );

        let value = serde_json::to_value(result).unwrap();
        assert_eq!(value["total_references"], 2);
        assert_eq!(value["returned_references"], 2);
        assert_eq!(value["groups"][0]["project_relative_path"], "src/lib.rs");
        assert_eq!(
            value["groups"][0]["references"].as_array().unwrap().len(),
            2
        );
    }

    #[test]
    fn references_apply_per_symbol_limit() {
        let result = group_references(
            vec![
                unavailable_location("/workspace/src/lib.rs", 2),
                unavailable_location("/workspace/src/lib.rs", 4),
                unavailable_location("/workspace/src/lib.rs", 6),
            ],
            None,
            3,
            &[std::path::PathBuf::from("/workspace")],
            &std::collections::HashMap::new(),
            SemanticResultLimits {
                total: 10,
                per_file: 1,
                per_symbol: 1,
            },
            false,
        );

        assert_eq!(result.total_references, 3);
        assert_eq!(result.returned_references, 1);
        assert_eq!(result.groups[0].references.len(), 1);
        assert!(result.truncated);
    }

    #[test]
    fn references_apply_total_and_per_file_limits() {
        let roots = [std::path::PathBuf::from("/workspace")];
        let locations = || {
            vec![
                unavailable_location("/workspace/src/a.rs", 2),
                unavailable_location("/workspace/src/a.rs", 4),
                unavailable_location("/workspace/src/b.rs", 2),
            ]
        };
        let total_limited = group_references(
            locations(),
            None,
            3,
            &roots,
            &std::collections::HashMap::new(),
            SemanticResultLimits {
                total: 1,
                per_file: 10,
                per_symbol: 10,
            },
            false,
        );
        let file_limited = group_references(
            locations(),
            None,
            3,
            &roots,
            &std::collections::HashMap::new(),
            SemanticResultLimits {
                total: 10,
                per_file: 1,
                per_symbol: 10,
            },
            false,
        );

        assert_eq!(total_limited.returned_references, 1);
        assert_eq!(file_limited.returned_references, 2);
        assert!(total_limited.truncated && file_limited.truncated);
    }

    #[test]
    fn references_group_same_line_uses_by_enclosing_symbol() {
        let mut first = unavailable_location("/workspace/src/lib.rs", 4);
        first.range.start.character = 3;
        let mut second = unavailable_location("/workspace/src/lib.rs", 4);
        second.range.start.character = 9;
        let symbol = Symbol {
            name: "caller".to_owned(),
            kind: "Function".to_owned(),
            range: Range {
                start: Position2D {
                    line: 1,
                    character: 1,
                },
                end: Position2D {
                    line: 8,
                    character: 1,
                },
            },
            selection_range: Range {
                start: Position2D {
                    line: 1,
                    character: 4,
                },
                end: Position2D {
                    line: 1,
                    character: 10,
                },
            },
            symbol_handle: None,
            container_name: None,
            match_class: None,
            score: None,
            source: None,
            is_private: false,
            is_test: false,
            children: None,
        };
        let symbols =
            std::collections::HashMap::from([("/workspace/src/lib.rs".to_owned(), vec![symbol])]);

        let result = group_references(
            vec![second, first],
            None,
            2,
            &[std::path::PathBuf::from("/workspace")],
            &symbols,
            SemanticResultLimits::default(),
            false,
        );

        assert_eq!(result.groups[0].enclosing_symbol.as_deref(), Some("caller"));
        assert_eq!(
            result.groups[0].references[0]
                .location
                .range
                .start
                .character,
            3
        );
        assert_eq!(
            result.groups[0].references[1]
                .location
                .range
                .start
                .character,
            9
        );
    }
}
