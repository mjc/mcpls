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
    NavigationKind, Position2D, Range, ReferenceGroup, ReferenceRole, ReferenceSnapshot,
    ReferenceSource, ReferenceSourceChunk, ReferenceUse, ReferencesResult, SemanticResultLimits,
    SourceContext, Symbol,
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
        self.handle_references_page(
            file_path,
            line,
            character,
            include_declaration,
            limits,
            None,
        )
        .await
    }

    /// Handle one deterministic page of references.
    ///
    /// # Errors
    ///
    /// Returns an error if the LSP request fails, the file cannot be opened,
    /// or the routed server does not advertise `referencesProvider` support.
    #[allow(clippy::too_many_lines)]
    pub async fn handle_references_page(
        &self,
        file_path: String,
        line: u32,
        character: u32,
        include_declaration: bool,
        limits: SemanticResultLimits,
        page_offset: Option<usize>,
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
            Some(
                ctx.location(&self.workspace_roots, location, &mut budget)
                    .await,
            )
        } else {
            None
        };
        for loc in locations {
            result_locations.push(ctx.location(&self.workspace_roots, loc, &mut budget).await);
        }
        let enclosing_symbols = self.reference_symbols(&result_locations).await;
        let result = if let Some(offset) = page_offset {
            group_references_page(
                result_locations,
                declaration,
                total_references,
                &self.workspace_roots,
                &enclosing_symbols,
                limits,
                offset,
                budget.truncated(),
            )
        } else {
            group_references(
                result_locations,
                declaration,
                total_references,
                &self.workspace_roots,
                &enclosing_symbols,
                limits,
                budget.truncated(),
            )
        };

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
    locations: Vec<Location>,
    declaration: Option<Location>,
    total_references: usize,
    workspace_roots: &[std::path::PathBuf],
    enclosing_symbols: &std::collections::HashMap<String, Vec<Symbol>>,
    limits: SemanticResultLimits,
    source_truncated: bool,
) -> ReferencesResult {
    let mut groups = collect_reference_groups(locations, workspace_roots, enclosing_symbols);
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
        group.locations.truncate(available);
        *file_count += group.locations.len();
        returned_references += group.locations.len();
        !group.locations.is_empty()
    });
    let returned_groups = groups.len();
    let omitted_groups = total_groups.saturating_sub(returned_groups);
    ReferencesResult {
        provider: "standard_lsp".to_owned(),
        groups: groups.into_iter().map(compact_reference_group).collect(),
        declaration,
        total_references,
        returned_references,
        total_groups,
        returned_groups,
        omitted_groups,
        limits,
        truncated: source_truncated || returned_references < total_references,
        next_cursor: None,
    }
}

fn collect_reference_groups(
    mut locations: Vec<Location>,
    workspace_roots: &[std::path::PathBuf],
    enclosing_symbols: &std::collections::HashMap<String, Vec<Symbol>>,
) -> Vec<ReferenceLocationGroup> {
    locations.sort_by(|left, right| {
        reference_source_order(left, workspace_roots)
            .cmp(&reference_source_order(right, workspace_roots))
            .then_with(|| left.path.cmp(&right.path))
            .then_with(|| left.range.start.line.cmp(&right.range.start.line))
            .then_with(|| left.range.start.character.cmp(&right.range.start.character))
    });
    let mut groups: Vec<ReferenceLocationGroup> = Vec::new();
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
        if let Some(group) = groups.last_mut().filter(|group| {
            group.project_relative_path == project_relative_path
                && group.uri == location.uri
                && group.enclosing_symbol == enclosing_symbol
        }) {
            group.locations.push(location);
        } else {
            groups.push(ReferenceLocationGroup {
                project_relative_path,
                uri: location.uri.clone(),
                path: location.path.clone(),
                enclosing_symbol,
                locations: vec![location],
            });
        }
    }
    groups
}

#[allow(clippy::too_many_arguments)]
fn group_references_page(
    locations: Vec<Location>,
    declaration: Option<Location>,
    total_references: usize,
    workspace_roots: &[std::path::PathBuf],
    enclosing_symbols: &std::collections::HashMap<String, Vec<Symbol>>,
    limits: SemanticResultLimits,
    page_offset: usize,
    source_truncated: bool,
) -> ReferencesResult {
    let all_groups = collect_reference_groups(locations, workspace_roots, enclosing_symbols);
    let total_groups = all_groups.len();
    let page_size = limits.total.max(1);
    let start = page_offset.min(total_references);
    let end = start.saturating_add(page_size).min(total_references);
    let mut index = 0usize;
    let declaration = declaration.filter(|_| {
        let included = (start..end).contains(&index);
        index = index.saturating_add(1);
        included
    });
    let mut page_groups: Vec<ReferenceLocationGroup> = Vec::new();
    for group in all_groups {
        let mut references = Vec::new();
        for reference in group.locations {
            if (start..end).contains(&index) {
                references.push(reference);
            }
            index = index.saturating_add(1);
        }
        if !references.is_empty() {
            page_groups.push(ReferenceLocationGroup {
                project_relative_path: group.project_relative_path,
                uri: group.uri,
                path: group.path,
                enclosing_symbol: group.enclosing_symbol,
                locations: references,
            });
        }
    }
    let returned_groups = page_groups.len();
    ReferencesResult {
        provider: "standard_lsp".to_owned(),
        groups: page_groups
            .into_iter()
            .map(compact_reference_group)
            .collect(),
        declaration,
        total_references,
        returned_references: end.saturating_sub(start),
        total_groups,
        returned_groups,
        omitted_groups: total_groups.saturating_sub(returned_groups),
        limits,
        truncated: source_truncated || end < total_references,
        next_cursor: (end < total_references).then(|| end.to_string()),
    }
}

struct ReferenceLocationGroup {
    project_relative_path: String,
    uri: String,
    path: Option<String>,
    enclosing_symbol: Option<String>,
    locations: Vec<Location>,
}

fn compact_reference_group(group: ReferenceLocationGroup) -> ReferenceGroup {
    let mut source = ReferenceSource::default();
    let references = group
        .locations
        .into_iter()
        .map(|location| {
            collect_reference_source(&mut source, &location.source);
            let snapshot = match &location.source {
                SourceContext::Available(frame) => Some(ReferenceSnapshot {
                    path: frame.path.clone(),
                    document_version: frame.document_version,
                    content_hash: frame.content_hash.clone(),
                }),
                SourceContext::Deferred { .. } | SourceContext::Unavailable { .. } => None,
            };
            ReferenceUse {
                range: range_tuple(&location.range),
                role: ReferenceRole::Unknown,
                symbol_handle: location.symbol_handle,
                snapshot,
            }
        })
        .collect();
    ReferenceGroup {
        project_relative_path: group.project_relative_path,
        uri: group.uri,
        path: group.path,
        enclosing_symbol: group.enclosing_symbol,
        references,
        source,
    }
}

fn collect_reference_source(source: &mut ReferenceSource, context: &SourceContext) {
    match context {
        SourceContext::Available(frame) => {
            let has_chunks = !source.chunks.is_empty();
            let same_snapshot = !has_chunks
                || (source.content_hash.as_deref() == Some(&frame.content_hash)
                    && source.document_version == frame.document_version);
            if !has_chunks {
                source.content_hash = Some(frame.content_hash.clone());
                source.document_version = frame.document_version;
            } else if !same_snapshot {
                if let Some(previous_hash) = source.content_hash.take() {
                    let previous_version = source.document_version.take();
                    for chunk in &mut source.chunks {
                        chunk.content_hash = Some(previous_hash.clone());
                        chunk.document_version = previous_version;
                    }
                }
                source.document_version = None;
            }
            let mixed_snapshot = has_chunks && (!same_snapshot || source.content_hash.is_none());
            let chunk = ReferenceSourceChunk {
                lines: [frame.range.start.line, frame.range.end.line],
                text: frame.text.clone(),
                content_hash: mixed_snapshot.then(|| frame.content_hash.clone()),
                document_version: mixed_snapshot.then_some(frame.document_version).flatten(),
            };
            if same_snapshot {
                push_reference_chunk(&mut source.chunks, chunk);
            } else {
                source.chunks.push(chunk);
            }
        }
        SourceContext::Deferred { resource } => {
            if !source
                .deferred
                .iter()
                .any(|known| known.uri == resource.uri)
            {
                source.deferred.push(resource.clone());
            }
        }
        SourceContext::Unavailable { reason } => {
            if !source.unavailable.contains(reason) {
                source.unavailable.push(*reason);
            }
        }
    }
}

fn push_reference_chunk(chunks: &mut Vec<ReferenceSourceChunk>, chunk: ReferenceSourceChunk) {
    let Some(previous) = chunks.last_mut() else {
        chunks.push(chunk);
        return;
    };
    if chunk.lines[0] > previous.lines[1].saturating_add(1) {
        chunks.push(chunk);
        return;
    }
    let skipped = previous.lines[1]
        .saturating_add(1)
        .saturating_sub(chunk.lines[0]) as usize;
    for line in chunk.text.lines().skip(skipped) {
        previous.text.push_str(line);
        previous.text.push('\n');
    }
    previous.lines[1] = previous.lines[1].max(chunk.lines[1]);
}

const fn range_tuple(range: &Range) -> [u32; 4] {
    [
        range.start.line,
        range.start.character,
        range.end.line,
        range.end.character,
    ]
}

fn reference_source_order(location: &Location, roots: &[std::path::PathBuf]) -> u8 {
    let Some(path) = location.path.as_deref().map(std::path::Path::new) else {
        return 2;
    };
    if !roots.iter().any(|root| path.starts_with(root)) {
        return 2;
    }
    if path
        .components()
        .any(|component| matches!(component.as_os_str().to_str(), Some("target" | "generated")))
    {
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
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::format_collect)]
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

    fn available_location(path: &str, line: u32) -> Location {
        let mut location = unavailable_location(path, line);
        location.source =
            super::super::dto::SourceContext::Available(super::super::dto::SourceFrame {
                path: path.to_owned(),
                uri: format!("file://{path}"),
                range: Range {
                    start: Position2D {
                        line: line.saturating_sub(5).max(1),
                        character: 1,
                    },
                    end: Position2D {
                        line: line.saturating_add(6),
                        character: 1,
                    },
                },
                highlighted_range: location.range.clone(),
                text: (line.saturating_sub(5).max(1)..=line.saturating_add(6))
                    .map(|line| format!("{line:>4} | let value = target();\n"))
                    .collect(),
                language_id: Some("rust".to_owned()),
                document_version: Some(1),
                content_hash: "a".repeat(64),
                returned_lines: 12,
                total_lines: 500,
                returned_bytes: 384,
                total_bytes: 16_000,
                truncated: false,
                resource: None,
            });
        location
    }

    fn symbol(name: &str, start: u32, end: u32, children: Option<Vec<Symbol>>) -> Symbol {
        let range = Range {
            start: Position2D {
                line: start,
                character: 1,
            },
            end: Position2D {
                line: end,
                character: 1,
            },
        };
        Symbol {
            name: name.to_owned(),
            kind: "Function".to_owned(),
            selection_range: range.clone(),
            range,
            symbol_handle: None,
            parent_symbol_handle: None,
            container_name: None,
            match_class: None,
            score: None,
            source: None,
            is_private: false,
            is_test: false,
            children,
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
        assert_eq!(value["groups"][0]["references"][0]["role"], "unknown");
        assert_eq!(
            value["groups"][0]["references"].as_array().unwrap().len(),
            2
        );
    }

    #[test]
    fn adjacent_reference_context_serialization_stays_compact() {
        let result = group_references(
            (100..150)
                .map(|line| available_location("/workspace/src/lib.rs", line))
                .collect(),
            None,
            50,
            &[std::path::PathBuf::from("/workspace")],
            &std::collections::HashMap::new(),
            SemanticResultLimits {
                total: 50,
                per_file: 50,
                per_symbol: 50,
            },
            false,
        );

        let json = serde_json::to_vec(&result).unwrap();

        assert!(json.len() < 5_000, "{} bytes", json.len());
    }

    #[test]
    fn reference_context_merges_only_neighboring_windows() {
        let adjacent = group_references(
            vec![
                available_location("/workspace/src/lib.rs", 100),
                available_location("/workspace/src/lib.rs", 101),
            ],
            None,
            2,
            &[std::path::PathBuf::from("/workspace")],
            &std::collections::HashMap::new(),
            SemanticResultLimits::default(),
            false,
        );
        let sparse = group_references(
            vec![
                available_location("/workspace/src/lib.rs", 100),
                available_location("/workspace/src/lib.rs", 200),
            ],
            None,
            2,
            &[std::path::PathBuf::from("/workspace")],
            &std::collections::HashMap::new(),
            SemanticResultLimits::default(),
            false,
        );

        assert_eq!(adjacent.groups[0].references.len(), 2);
        assert_eq!(adjacent.groups[0].source.chunks.len(), 1);
        assert_eq!(adjacent.groups[0].source.chunks[0].lines, [95, 107]);
        assert_eq!(sparse.groups[0].references.len(), 2);
        assert_eq!(sparse.groups[0].source.chunks.len(), 2);
    }

    #[test]
    fn adjacent_reference_context_keeps_the_boundary_line() {
        let result = group_references(
            vec![
                available_location("/workspace/src/lib.rs", 100),
                available_location("/workspace/src/lib.rs", 112),
            ],
            None,
            2,
            &[std::path::PathBuf::from("/workspace")],
            &std::collections::HashMap::new(),
            SemanticResultLimits::default(),
            false,
        );

        let chunk = &result.groups[0].source.chunks[0];
        assert_eq!(result.groups[0].source.chunks.len(), 1);
        assert!(chunk.text.contains(" 107 |"), "{}", chunk.text);
        assert!(chunk.text.contains(" 118 |"), "{}", chunk.text);
    }

    #[test]
    fn mixed_snapshot_chunks_keep_per_chunk_provenance() {
        let first = available_location("/workspace/src/lib.rs", 100);
        let mut second = available_location("/workspace/src/lib.rs", 200);
        let SourceContext::Available(frame) = &mut second.source else {
            panic!("source unavailable")
        };
        frame.content_hash = "b".repeat(64);
        frame.document_version = Some(2);

        let result = group_references(
            vec![first, second],
            None,
            2,
            &[std::path::PathBuf::from("/workspace")],
            &std::collections::HashMap::new(),
            SemanticResultLimits::default(),
            false,
        );

        let source = &result.groups[0].source;
        let first_hash = "a".repeat(64);
        let second_hash = "b".repeat(64);
        assert!(source.content_hash.is_none());
        assert_eq!(source.chunks.len(), 2);
        assert_eq!(
            source.chunks[0].content_hash.as_deref(),
            Some(first_hash.as_str())
        );
        assert_eq!(source.chunks[0].document_version, Some(1));
        assert_eq!(
            source.chunks[1].content_hash.as_deref(),
            Some(second_hash.as_str())
        );
        assert_eq!(source.chunks[1].document_version, Some(2));
    }

    #[test]
    fn identical_relative_paths_from_distinct_roots_do_not_coalesce() {
        let result = group_references(
            vec![
                unavailable_location("/workspace/a/src/lib.rs", 1),
                unavailable_location("/workspace/b/src/lib.rs", 1),
            ],
            None,
            2,
            &[
                std::path::PathBuf::from("/workspace/a"),
                std::path::PathBuf::from("/workspace/b"),
            ],
            &std::collections::HashMap::new(),
            SemanticResultLimits::default(),
            false,
        );

        assert_eq!(result.groups.len(), 2);
        assert_eq!(result.groups[0].project_relative_path, "src/lib.rs");
        assert_eq!(result.groups[1].project_relative_path, "src/lib.rs");
        assert_ne!(result.groups[0].uri, result.groups[1].uri);
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
        let symbol = symbol(
            "impl Trait for Type",
            1,
            8,
            Some(vec![symbol("caller", 2, 7, None)]),
        );
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
        assert_eq!(result.groups[0].references[0].range[1], 3);
        assert_eq!(result.groups[0].references[1].range[1], 9);
    }

    #[test]
    fn references_order_production_before_tests_generated_and_external() {
        let result = group_references(
            vec![
                unavailable_location("/outside/dependency.rs", 1),
                unavailable_location("/workspace/target/generated.rs", 1),
                unavailable_location("/workspace/tests/use.rs", 1),
                unavailable_location("/workspace/src/lib.rs", 1),
            ],
            None,
            4,
            &[std::path::PathBuf::from("/workspace")],
            &std::collections::HashMap::new(),
            SemanticResultLimits::default(),
            false,
        );
        let paths = result
            .groups
            .iter()
            .map(|group| group.project_relative_path.as_str())
            .collect::<Vec<_>>();

        assert_eq!(
            paths,
            [
                "src/lib.rs",
                "tests/use.rs",
                "/outside/dependency.rs",
                "target/generated.rs"
            ]
        );
    }

    #[test]
    fn references_page_exhaustion_returns_every_compact_location_once() {
        let roots = [std::path::PathBuf::from("/workspace")];
        let locations = vec![
            unavailable_location("/workspace/src/a.rs", 2),
            unavailable_location("/workspace/src/a.rs", 4),
            unavailable_location("/workspace/src/b.rs", 2),
        ];
        let limits = SemanticResultLimits {
            total: 2,
            per_file: 1,
            per_symbol: 1,
        };
        let first = group_references_page(
            locations.clone(),
            Some(unavailable_location("/workspace/src/lib.rs", 1)),
            4,
            &roots,
            &std::collections::HashMap::new(),
            limits,
            0,
            false,
        );
        let second = group_references_page(
            locations,
            Some(unavailable_location("/workspace/src/lib.rs", 1)),
            4,
            &roots,
            &std::collections::HashMap::new(),
            limits,
            2,
            false,
        );
        assert_eq!(first.returned_references, 2);
        assert_eq!(first.next_cursor.as_deref(), Some("2"));
        assert_eq!(second.returned_references, 2);
        assert_eq!(second.next_cursor, None);
        let mut lines = first
            .groups
            .into_iter()
            .chain(second.groups)
            .flat_map(|group| group.references)
            .map(|reference| reference.range[0])
            .collect::<Vec<_>>();
        lines.extend(
            first
                .declaration
                .into_iter()
                .chain(second.declaration)
                .map(|location| location.range.start.line),
        );
        lines.sort_unstable();
        assert_eq!(lines, vec![1, 2, 2, 4]);
    }
}
