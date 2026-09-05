//! Call hierarchy prepare/incoming/outgoing handlers.

use lsp_types::{
    CallHierarchyIncomingCall, CallHierarchyIncomingCallsParams, CallHierarchyItem,
    CallHierarchyOutgoingCall, CallHierarchyOutgoingCallsParams,
    CallHierarchyPrepareParams as LspCallHierarchyPrepareParams, PartialResultParams,
    TextDocumentIdentifier, TextDocumentPositionParams, WorkDoneProgressParams,
};
use sha2::{Digest, Sha256};

use super::Translator;
use super::dto::{
    CallHierarchyItemResult, CallHierarchyPrepareResult, CallSite, IncomingCall,
    IncomingCallsResult, NavigationKind, OutgoingCall, OutgoingCallsResult, SemanticResultLimits,
};
use super::encoding_ctx::EncodingCtx;
use super::routing::MAX_POSITION_VALUE;
use crate::config::ToolKind;
use crate::error::{Error, Result};

const CALL_PAGE_GROUPS: usize = 64;

fn call_page_bounds<T: serde::Serialize>(
    calls: &[T],
    page_token: Option<&str>,
    kind: &str,
) -> Result<(usize, usize, String)> {
    let snapshot_identity = format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(calls).unwrap_or_default())
    );
    let (group, site) = match page_token {
        Some(token) => {
            let mut parts = token.split(':');
            let identity = parts.next();
            let group = parts.next();
            let site = parts.next();
            if identity != Some(snapshot_identity.as_str()) || parts.next().is_some() {
                return Err(Error::InvalidToolParams(format!(
                    "{kind} page_token belongs to a different snapshot"
                )));
            }
            let group = group
                .and_then(|value| value.parse().ok())
                .ok_or_else(|| Error::InvalidToolParams(format!("invalid {kind} page_token")))?;
            let site = site
                .and_then(|value| value.parse().ok())
                .ok_or_else(|| Error::InvalidToolParams(format!("invalid {kind} page_token")))?;
            (group, site)
        }
        None => (0, 0),
    };
    if group > calls.len() {
        return Err(Error::InvalidToolParams(format!(
            "{kind} page_token is outside the provider snapshot"
        )));
    }
    Ok((group, site, snapshot_identity))
}

/// Split a prepared item snapshot into a deterministic page and remainder.
pub fn page_items<T>(mut items: Vec<T>, page_size: usize) -> (Vec<T>, Option<Vec<T>>) {
    let split_at = items.len().min(page_size.max(1));
    let remaining = items.split_off(split_at);
    (items, (!remaining.is_empty()).then_some(remaining))
}

/// Whether a server's capabilities advertise `callHierarchyProvider` support.
///
/// Shared by `handle_call_hierarchy_prepare`, `handle_incoming_calls`, and
/// `handle_outgoing_calls`, which all gate on the same capability field.
const fn call_hierarchy_provider_supported(caps: &lsp_types::ServerCapabilities) -> bool {
    matches!(
        caps.call_hierarchy_provider,
        Some(
            lsp_types::CallHierarchyServerCapability::Simple(true)
                | lsp_types::CallHierarchyServerCapability::Options(_)
        )
    )
}

/// Parsed form of an MCP-facing `CallHierarchyItemResult` JSON value (1-based
/// coordinates), before its ranges are converted back to the routed server's
/// negotiated encoding -- which requires resolving that server first (from
/// [`Self::uri`]), so that step is left to callers via
/// [`call_hierarchy_item_to_lsp`].
struct ParsedCallHierarchyItem {
    uri: lsp_types::Uri,
    mcp: CallHierarchyItemResult,
}

/// Deserialize an MCP-facing `CallHierarchyItemResult` JSON value and parse
/// its URI.
///
/// MCP clients receive `CallHierarchyItemResult` from `prepare_call_hierarchy`
/// and pass it back opaquely to `get_incoming_calls` / `get_outgoing_calls`.
fn parse_mcp_call_hierarchy_item(item: serde_json::Value) -> Result<ParsedCallHierarchyItem> {
    let mcp: CallHierarchyItemResult = serde_json::from_value(item)
        .map_err(|e| Error::InvalidToolParams(format!("Invalid call hierarchy item: {e}")))?;

    let uri = mcp.uri.parse::<lsp_types::Uri>().map_err(|e| {
        Error::InvalidToolParams(format!("Invalid URI in call hierarchy item: {e}"))
    })?;

    Ok(ParsedCallHierarchyItem { uri, mcp })
}

/// Convert a parsed MCP call hierarchy item (1-based coordinates) into a
/// `lsp_types::CallHierarchyItem` (0-based, in `ctx`'s negotiated encoding).
async fn call_hierarchy_item_to_lsp(
    parsed: ParsedCallHierarchyItem,
    ctx: &EncodingCtx,
) -> CallHierarchyItem {
    let ParsedCallHierarchyItem { uri, mcp } = parsed;

    // Round-trip via serde: `convert_call_hierarchy_item` stored the kind as a u32
    // by serialising `SymbolKind`; we reverse this to reconstruct the same value.
    let kind: lsp_types::SymbolKind = serde_json::from_value(serde_json::json!(mcp.kind))
        .unwrap_or(lsp_types::SymbolKind::FUNCTION);
    let range = ctx.denormalize_range(&uri, &mcp.range).await;
    let selection_range = ctx.denormalize_range(&uri, &mcp.selection_range).await;

    CallHierarchyItem {
        name: mcp.name,
        kind,
        tags: None,
        detail: mcp.detail,
        uri,
        range,
        selection_range,
        data: mcp.data,
    }
}

/// Convert LSP call hierarchy item to MCP call hierarchy item.
async fn convert_call_hierarchy_item(
    item: CallHierarchyItem,
    ctx: &EncodingCtx,
    workspace_roots: &[std::path::PathBuf],
    budget: &mut super::source_context::SourceBudget,
) -> CallHierarchyItemResult {
    let range = ctx.normalize_range(&item.uri, item.range).await;
    let selection_range = ctx.normalize_range(&item.uri, item.selection_range).await;
    let source = ctx
        .source_context(workspace_roots, &item.uri, selection_range.clone(), budget)
        .await;

    CallHierarchyItemResult {
        name: item.name,
        kind: serde_json::to_value(item.kind)
            .ok()
            .and_then(|v| v.as_u64())
            .and_then(|n| u32::try_from(n).ok())
            .unwrap_or(0),
        detail: item.detail,
        path: crate::bridge::state::uri_to_path(&item.uri)
            .map(|path| path.to_string_lossy().into_owned()),
        uri: item.uri.to_string(),
        range,
        selection_range,
        source: Some(source),
        symbol_handle: None,
        data: item.data,
    }
}

impl Translator {
    /// Handle call hierarchy prepare request.
    ///
    /// # Errors
    ///
    /// Returns an error if the LSP request fails, the file cannot be opened,
    /// or the routed server does not advertise `callHierarchyProvider` support.
    pub async fn handle_call_hierarchy_prepare(
        &self,
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

        let (server_id, client, uri) = self
            .prepare_gated_document(
                &file_path,
                ToolKind::CallHierarchy,
                "callHierarchyProvider",
                call_hierarchy_provider_supported,
            )
            .await?;
        let ctx = self.encoding_ctx(&server_id);
        let lsp_position = ctx.to_lsp(&uri, line, character).await;

        let params = LspCallHierarchyPrepareParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
                position: lsp_position,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
        };

        let response: Option<Vec<CallHierarchyItem>> = client
            .request(
                "textDocument/prepareCallHierarchy",
                params,
                client.request_timeout(),
            )
            .await?;

        // Pre-allocate and build result
        let lsp_items = response.unwrap_or_default();
        let total_items = lsp_items.len();
        let mut items = Vec::with_capacity(total_items);
        let mut source_budget = super::source_context::SourceBudget::default();
        for item in lsp_items {
            items.push(
                convert_call_hierarchy_item(item, &ctx, &self.workspace_roots, &mut source_budget)
                    .await,
            );
        }

        Ok(CallHierarchyPrepareResult {
            provider: "standard_lsp".to_owned(),
            kind: NavigationKind::CallHierarchy,
            total_items,
            returned_items: items.len(),
            next_cursor: None,
            truncated: source_budget.truncated(),
            items,
        })
    }

    /// Handle incoming calls request.
    ///
    /// # Errors
    ///
    /// Returns an error if the LSP request fails, the item is invalid, or the
    /// routed server does not advertise `callHierarchyProvider` support.
    pub async fn handle_incoming_calls(
        &self,
        item: serde_json::Value,
        limits: SemanticResultLimits,
        page_token: Option<String>,
    ) -> Result<IncomingCallsResult> {
        // Deserialize as our own type (1-based coords).
        let parsed = parse_mcp_call_hierarchy_item(item)?;

        // Parse and validate the URI. Resolved with the same ToolKind as
        // `handle_call_hierarchy_prepare` -- the opaque item this call
        // receives is only meaningful to the server that produced it, and
        // that server is guaranteed to be the same one `prepare` synced the
        // document to since both resolve via the same (language, tool) route.
        let path = self.parse_file_uri(&parsed.uri)?;
        let (server_id, client) = self
            .resolve_client_for_file(&path, ToolKind::CallHierarchy)
            .await?;
        self.require_capability(
            &server_id,
            "callHierarchyProvider",
            call_hierarchy_provider_supported,
        )?;
        let ctx = self.encoding_ctx(&server_id);
        let lsp_item = call_hierarchy_item_to_lsp(parsed, &ctx).await;

        let params = CallHierarchyIncomingCallsParams {
            item: lsp_item,
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };

        let response: Option<Vec<CallHierarchyIncomingCall>> = client
            .request(
                "callHierarchy/incomingCalls",
                params,
                client.request_timeout(),
            )
            .await?;

        // Pre-allocate and build result
        let mut lsp_calls = response.unwrap_or_default();
        stable_sort_incoming_calls(&mut lsp_calls, &self.workspace_roots);
        let total_calls = lsp_calls.len();
        let total_call_sites = lsp_calls.iter().map(|call| call.from_ranges.len()).sum();
        let (group_offset, site_offset, snapshot_identity) =
            call_page_bounds(&lsp_calls, page_token.as_deref(), "incoming_calls")?;
        let prior_call_sites: usize = lsp_calls
            .iter()
            .take(group_offset)
            .map(|call| call.from_ranges.len())
            .sum();
        let mut calls = Vec::with_capacity(total_calls.min(limits.total));
        let mut source_budget = super::source_context::SourceBudget::default();
        let mut next_cursor = None;
        let mut consumed_calls = group_offset;
        let mut consumed_call_sites = prior_call_sites + site_offset;

        for (group_index, call) in lsp_calls.into_iter().enumerate().skip(group_offset) {
            if calls.len() >= limits.total.min(CALL_PAGE_GROUPS) {
                next_cursor = Some(format!("{snapshot_identity}:{group_index}:0"));
                break;
            }
            // Per the LSP spec, `fromRanges` are ranges within the *caller's*
            // document (`call.from.uri`), not the queried item's document.
            let from_uri = call.from.uri.clone();
            let start_site = if group_index == group_offset {
                site_offset
            } else {
                0
            };
            let available_sites = call.from_ranges.len();
            if start_site > available_sites {
                return Err(Error::InvalidToolParams(
                    "incoming_calls page_token is outside the provider snapshot".to_owned(),
                ));
            }
            let site_limit = limits.per_symbol.max(1);
            let call_sites = {
                let mut sites =
                    Vec::with_capacity(available_sites.saturating_sub(start_site).min(site_limit));
                for range in call
                    .from_ranges
                    .into_iter()
                    .skip(start_site)
                    .take(site_limit)
                {
                    let range = ctx.normalize_range(&from_uri, range).await;
                    let source = ctx
                        .source_context(
                            &self.workspace_roots,
                            &from_uri,
                            range.clone(),
                            &mut source_budget,
                        )
                        .await;
                    sites.push(CallSite { range, source });
                }
                sites
            };

            calls.push(IncomingCall {
                from: convert_call_hierarchy_item(
                    call.from,
                    &ctx,
                    &self.workspace_roots,
                    &mut source_budget,
                )
                .await,
                call_sites,
            });
            let returned_sites = calls.last().map_or(0, |call| call.call_sites.len());
            consumed_call_sites += returned_sites;
            if start_site + returned_sites < available_sites {
                next_cursor = Some(format!(
                    "{snapshot_identity}:{group_index}:{}",
                    start_site + returned_sites
                ));
                break;
            }
            consumed_calls = group_index + 1;
        }

        let returned_calls = calls.len();
        let returned_call_sites = calls.iter().map(|call| call.call_sites.len()).sum();
        Ok(IncomingCallsResult {
            provider: "standard_lsp".to_owned(),
            calls,
            total_calls,
            returned_calls,
            remaining_calls: total_calls.saturating_sub(consumed_calls),
            total_call_sites,
            returned_call_sites,
            remaining_call_sites: total_call_sites.saturating_sub(consumed_call_sites),
            snapshot_identity,
            next_cursor,
            omitted_groups: total_calls.saturating_sub(returned_calls),
            truncated: source_budget.truncated()
                || returned_calls < total_calls
                || returned_call_sites < total_call_sites,
            limits,
        })
    }

    /// Handle outgoing calls request.
    ///
    /// # Errors
    ///
    /// Returns an error if the LSP request fails, the item is invalid, or the
    /// routed server does not advertise `callHierarchyProvider` support.
    pub async fn handle_outgoing_calls(
        &self,
        item: serde_json::Value,
        limits: SemanticResultLimits,
        page_token: Option<String>,
    ) -> Result<OutgoingCallsResult> {
        // Deserialize as our own type (1-based coords).
        let parsed = parse_mcp_call_hierarchy_item(item)?;

        // Parse and validate the URI. Same ToolKind/route as `prepare` and
        // `handle_incoming_calls` -- see that function's comment.
        let path = self.parse_file_uri(&parsed.uri)?;
        let (server_id, client) = self
            .resolve_client_for_file(&path, ToolKind::CallHierarchy)
            .await?;
        self.require_capability(
            &server_id,
            "callHierarchyProvider",
            call_hierarchy_provider_supported,
        )?;
        let ctx = self.encoding_ctx(&server_id);
        // Per the LSP spec, an outgoing call's `fromRanges` are ranges within
        // the *queried* item's own document, not the callee's (`call.to.uri`).
        let source_uri = parsed.uri.clone();
        let lsp_item = call_hierarchy_item_to_lsp(parsed, &ctx).await;

        let params = CallHierarchyOutgoingCallsParams {
            item: lsp_item,
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };

        let response: Option<Vec<CallHierarchyOutgoingCall>> = client
            .request(
                "callHierarchy/outgoingCalls",
                params,
                client.request_timeout(),
            )
            .await?;

        // Pre-allocate and build result
        let mut lsp_calls = response.unwrap_or_default();
        stable_sort_outgoing_calls(&mut lsp_calls, &self.workspace_roots);
        let total_calls = lsp_calls.len();
        let total_call_sites = lsp_calls.iter().map(|call| call.from_ranges.len()).sum();
        let (group_offset, site_offset, snapshot_identity) =
            call_page_bounds(&lsp_calls, page_token.as_deref(), "outgoing_calls")?;
        let prior_call_sites: usize = lsp_calls
            .iter()
            .take(group_offset)
            .map(|call| call.from_ranges.len())
            .sum();
        let mut calls = Vec::with_capacity(total_calls.min(limits.total));
        let mut source_budget = super::source_context::SourceBudget::default();
        let mut next_cursor = None;
        let mut consumed_calls = group_offset;
        let mut consumed_call_sites = prior_call_sites + site_offset;

        for (group_index, call) in lsp_calls.into_iter().enumerate().skip(group_offset) {
            if calls.len() >= limits.total.min(CALL_PAGE_GROUPS) {
                next_cursor = Some(format!("{snapshot_identity}:{group_index}:0"));
                break;
            }
            let start_site = if group_index == group_offset {
                site_offset
            } else {
                0
            };
            let available_sites = call.from_ranges.len();
            if start_site > available_sites {
                return Err(Error::InvalidToolParams(
                    "outgoing_calls page_token is outside the provider snapshot".to_owned(),
                ));
            }
            let site_limit = limits.per_symbol.max(1);
            let call_sites = {
                let mut sites =
                    Vec::with_capacity(available_sites.saturating_sub(start_site).min(site_limit));
                for range in call
                    .from_ranges
                    .into_iter()
                    .skip(start_site)
                    .take(site_limit)
                {
                    let range = ctx.normalize_range(&source_uri, range).await;
                    let source = ctx
                        .source_context(
                            &self.workspace_roots,
                            &source_uri,
                            range.clone(),
                            &mut source_budget,
                        )
                        .await;
                    sites.push(CallSite { range, source });
                }
                sites
            };

            calls.push(OutgoingCall {
                to: convert_call_hierarchy_item(
                    call.to,
                    &ctx,
                    &self.workspace_roots,
                    &mut source_budget,
                )
                .await,
                call_sites,
            });
            let returned_sites = calls.last().map_or(0, |call| call.call_sites.len());
            consumed_call_sites += returned_sites;
            if start_site + returned_sites < available_sites {
                next_cursor = Some(format!(
                    "{snapshot_identity}:{group_index}:{}",
                    start_site + returned_sites
                ));
                break;
            }
            consumed_calls = group_index + 1;
        }

        let returned_calls = calls.len();
        let returned_call_sites = calls.iter().map(|call| call.call_sites.len()).sum();
        Ok(OutgoingCallsResult {
            provider: "standard_lsp".to_owned(),
            calls,
            total_calls,
            returned_calls,
            remaining_calls: total_calls.saturating_sub(consumed_calls),
            total_call_sites,
            returned_call_sites,
            remaining_call_sites: total_call_sites.saturating_sub(consumed_call_sites),
            snapshot_identity,
            next_cursor,
            omitted_groups: total_calls.saturating_sub(returned_calls),
            truncated: source_budget.truncated()
                || returned_calls < total_calls
                || returned_call_sites < total_call_sites,
            limits,
        })
    }
}

fn source_order(uri: &lsp_types::Uri, roots: &[std::path::PathBuf]) -> u8 {
    let Some(path) = crate::bridge::state::uri_to_path(uri) else {
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

fn stable_sort_incoming_calls(
    calls: &mut [CallHierarchyIncomingCall],
    roots: &[std::path::PathBuf],
) {
    calls.sort_by(|left, right| {
        source_order(&left.from.uri, roots)
            .cmp(&source_order(&right.from.uri, roots))
            .then_with(|| left.from.uri.as_str().cmp(right.from.uri.as_str()))
            .then_with(|| left.from.range.start.line.cmp(&right.from.range.start.line))
    });
}

fn stable_sort_outgoing_calls(
    calls: &mut [CallHierarchyOutgoingCall],
    roots: &[std::path::PathBuf],
) {
    calls.sort_by(|left, right| {
        source_order(&left.to.uri, roots)
            .cmp(&source_order(&right.to.uri, roots))
            .then_with(|| left.to.uri.as_str().cmp(right.to.uri.as_str()))
            .then_with(|| left.to.range.start.line.cmp(&right.to.range.start.line))
    });
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::fs;
    use std::sync::Arc;
    use std::time::Duration;

    use tempfile::TempDir;
    use tokio::io::BufReader;
    use tokio::time::timeout;
    use url::Url;

    use super::*;

    fn assert_source_available(source: &super::super::dto::SourceContext) {
        assert!(matches!(
            source,
            super::super::dto::SourceContext::Available(_)
        ));
    }

    fn call_item(name: &str, uri: String) -> CallHierarchyItemResult {
        let range = Range {
            start: Position2D {
                line: 1,
                character: 1,
            },
            end: Position2D {
                line: 1,
                character: 4,
            },
        };
        CallHierarchyItemResult {
            name: name.to_owned(),
            kind: 12,
            detail: None,
            uri,
            path: None,
            selection_range: range.clone(),
            range,
            source: None,
            symbol_handle: None,
            data: None,
        }
    }
    #[test]
    fn prepared_items_page_without_duplicates() {
        let items = (0..3)
            .map(|index| call_item(&format!("item-{index}"), format!("file:///item-{index}.rs")))
            .collect();

        let (first, remaining) = page_items(items, 2);
        assert_eq!(
            first
                .iter()
                .map(|item| item.name.as_str())
                .collect::<Vec<_>>(),
            ["item-0", "item-1"]
        );

        let (second, remaining) = page_items(remaining.expect("next page"), 2);
        assert_eq!(
            second
                .iter()
                .map(|item| item.name.as_str())
                .collect::<Vec<_>>(),
            ["item-2"]
        );
        assert!(remaining.is_none());
    }

    use crate::bridge::translator::dto::{Position2D, Range};
    use crate::bridge::translator::testing::*;
    use crate::config::ServerId;

    #[tokio::test]
    async fn test_handle_call_hierarchy_prepare_invalid_position_zero() {
        let translator = Translator::new();
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
        let translator = Translator::new();
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
        let translator = Translator::new();
        let invalid_item = serde_json::json!({"invalid": "structure"});
        let result = translator
            .handle_incoming_calls(invalid_item, SemanticResultLimits::default(), None)
            .await;
        assert!(matches!(result, Err(Error::InvalidToolParams(_))));
    }

    #[tokio::test]
    async fn test_handle_outgoing_calls_invalid_json() {
        let translator = Translator::new();
        let invalid_item = serde_json::json!({"invalid": "structure"});
        let result = translator
            .handle_outgoing_calls(invalid_item, SemanticResultLimits::default(), None)
            .await;
        assert!(matches!(result, Err(Error::InvalidToolParams(_))));
    }

    #[tokio::test]
    async fn test_convert_call_hierarchy_item_kind_is_numeric() {
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
        let result = convert_call_hierarchy_item(
            item,
            &test_ctx(),
            &[],
            &mut crate::bridge::translator::source_context::SourceBudget::default(),
        )
        .await;
        // SymbolKind::FUNCTION is LSP integer 12
        assert_eq!(result.kind, 12u32);
        assert_eq!(result.name, "my_fn");
    }

    /// Per the LSP spec, an incoming call's `fromRanges` are ranges within
    /// the *caller's* document (`call.from.uri`), not the queried item's
    /// document -- `handle_incoming_calls` must convert them against
    /// `caller.rs`'s own content, not `queried.rs`'s. Uses a UTF-8-negotiated
    /// server and two files with different multibyte content, so converting
    /// against the wrong file's line text produces a different, wrong
    /// answer: `"aöb"` (caller) puts LSP byte offset 3 at UTF-16 column 3
    /// (`ö` is 2 UTF-8 bytes / 1 UTF-16 unit), while the ASCII `"abc"`
    /// (queried item) would put the same byte offset at column 4.
    #[tokio::test]
    async fn test_handle_incoming_calls_from_ranges_convert_against_callers_own_uri() {
        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from("rust");
        let caps = lsp_types::ServerCapabilities {
            call_hierarchy_provider: Some(lsp_types::CallHierarchyServerCapability::Simple(true)),
            ..Default::default()
        };
        let (translator, mut server) = translator_with_capabilities_and_encoding(
            &dir,
            &server_id,
            caps,
            lsp_types::PositionEncodingKind::UTF8,
        );

        let queried_path = dir.path().join("queried.rs");
        fs::write(&queried_path, "abc").unwrap();
        let queried_uri = Url::from_file_path(&queried_path).unwrap().to_string();

        let caller_path = dir.path().join("caller.rs");
        fs::write(&caller_path, "aöb").unwrap();
        let caller_uri = Url::from_file_path(&caller_path).unwrap().to_string();

        let item = call_item("queried_fn", queried_uri);

        let translator = Arc::new(translator);
        let handle = {
            let translator = Arc::clone(&translator);
            let item = serde_json::to_value(item).unwrap();
            tokio::spawn(async move {
                translator
                    .handle_incoming_calls(item, SemanticResultLimits::default(), None)
                    .await
            })
        };

        let mut wire = BufReader::new(&mut server.write_stdout);
        let request = read_framed_message(&mut wire).await;
        assert_eq!(request["method"], "callHierarchy/incomingCalls");

        write_response(
            &mut server.read_half_stdin,
            &request["id"],
            serde_json::json!([{
                "from": {
                    "name": "caller_fn",
                    "kind": 12,
                    "uri": caller_uri.clone(),
                    "range": {
                        "start": {"line": 0, "character": 0},
                        "end": {"line": 0, "character": 1}
                    },
                    "selectionRange": {
                        "start": {"line": 0, "character": 0},
                        "end": {"line": 0, "character": 1}
                    }
                },
                "fromRanges": [{
                    "start": {"line": 0, "character": 0},
                    "end": {"line": 0, "character": 3}
                }, {
                    "start": {"line": 0, "character": 1},
                    "end": {"line": 0, "character": 2}
                }]
            }]),
        )
        .await;

        let result = timeout(Duration::from_secs(2), handle)
            .await
            .expect("handler call should not hang")
            .unwrap()
            .unwrap();

        assert_eq!(result.calls.len(), 1);
        assert_eq!(result.calls[0].call_sites.len(), 2);
        assert_source_available(&result.calls[0].call_sites[0].source);
        let from_range = &result.calls[0].call_sites[0].range;
        assert_eq!(
            from_range.end.character, 3,
            "fromRanges must convert against the caller's own file (\"aöb\"), not the queried \
             item's (\"abc\") -- a byte offset of 3 is UTF-16 column 3 in the former, 4 in the \
             latter"
        );
    }

    #[tokio::test]
    async fn incoming_call_pages_reconstruct_call_sites_and_reject_mutation() {
        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from("rust");
        let caps = lsp_types::ServerCapabilities {
            call_hierarchy_provider: Some(lsp_types::CallHierarchyServerCapability::Simple(true)),
            ..Default::default()
        };
        let (translator, mut server) = translator_with_capabilities(&dir, &server_id, caps);
        let queried_path = dir.path().join("queried.rs");
        let caller_path = dir.path().join("caller.rs");
        fs::write(&queried_path, "fn queried() {}\n").unwrap();
        fs::write(
            &caller_path,
            "fn caller() { queried(); queried(); queried(); }\n",
        )
        .unwrap();
        let item = call_item(
            "queried_fn",
            Url::from_file_path(&queried_path).unwrap().to_string(),
        );
        let caller_uri = Url::from_file_path(&caller_path).unwrap().to_string();
        let response = |end: u32| {
            serde_json::json!([{
                "from": {
                    "name": "caller_fn",
                    "kind": 12,
                    "uri": caller_uri,
                    "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 6}},
                    "selectionRange": {"start": {"line": 0, "character": 3}, "end": {"line": 0, "character": 6}}
                },
                "fromRanges": [
                    {"start": {"line": 0, "character": 14}, "end": {"line": 0, "character": end}},
                    {"start": {"line": 0, "character": 25}, "end": {"line": 0, "character": 31}},
                    {"start": {"line": 0, "character": 36}, "end": {"line": 0, "character": 42}}
                ]
            }])
        };
        let limits = SemanticResultLimits {
            total: 1,
            per_file: 1,
            per_symbol: 1,
        };
        let translator = Arc::new(translator);
        let item = serde_json::to_value(item).unwrap();
        let mut wire = BufReader::new(&mut server.write_stdout);

        let first_task = {
            let translator = Arc::clone(&translator);
            let item = item.clone();
            tokio::spawn(async move { translator.handle_incoming_calls(item, limits, None).await })
        };
        let request = read_framed_message(&mut wire).await;
        write_response(&mut server.read_half_stdin, &request["id"], response(20)).await;
        let first = first_task.await.unwrap().unwrap();
        assert_eq!(first.total_calls, 1);
        assert_eq!(first.total_call_sites, 3);
        assert_eq!(first.returned_call_sites, 1);
        assert_eq!(first.remaining_calls, 1);
        assert_eq!(first.remaining_call_sites, 2);
        let cursor = first.next_cursor.clone().unwrap();

        let second_task = {
            let translator = Arc::clone(&translator);
            let item = item.clone();
            let cursor = cursor.clone();
            tokio::spawn(async move {
                translator
                    .handle_incoming_calls(item, limits, Some(cursor))
                    .await
            })
        };
        let request = read_framed_message(&mut wire).await;
        write_response(&mut server.read_half_stdin, &request["id"], response(20)).await;
        let second = second_task.await.unwrap().unwrap();
        assert_eq!(second.returned_call_sites, 1);
        assert_eq!(second.remaining_calls, 1);
        assert_eq!(second.remaining_call_sites, 1);
        let cursor = second.next_cursor.clone().unwrap();

        let third_task = {
            let translator = Arc::clone(&translator);
            tokio::spawn(async move {
                translator
                    .handle_incoming_calls(item, limits, Some(cursor))
                    .await
            })
        };
        let request = read_framed_message(&mut wire).await;
        write_response(&mut server.read_half_stdin, &request["id"], response(21)).await;
        let error = third_task.await.unwrap().unwrap_err();
        assert!(
            matches!(error, Error::InvalidToolParams(message) if message.contains("different snapshot"))
        );
    }

    #[tokio::test]
    async fn outgoing_call_pages_reconstruct_call_sites() {
        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from("rust");
        let caps = lsp_types::ServerCapabilities {
            call_hierarchy_provider: Some(lsp_types::CallHierarchyServerCapability::Simple(true)),
            ..Default::default()
        };
        let (translator, mut server) = translator_with_capabilities(&dir, &server_id, caps);
        let queried_path = dir.path().join("queried.rs");
        let callee_path = dir.path().join("callee.rs");
        fs::write(&queried_path, "fn queried() { callee(); callee(); }\n").unwrap();
        fs::write(&callee_path, "fn callee() {}\n").unwrap();
        let item = call_item(
            "queried_fn",
            Url::from_file_path(&queried_path).unwrap().to_string(),
        );
        let callee_uri = Url::from_file_path(&callee_path).unwrap().to_string();
        let response = serde_json::json!([{
            "to": {
                "name": "callee_fn",
                "kind": 12,
                "uri": callee_uri,
                "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 13}},
                "selectionRange": {"start": {"line": 0, "character": 3}, "end": {"line": 0, "character": 8}}
            },
            "fromRanges": [
                {"start": {"line": 0, "character": 15}, "end": {"line": 0, "character": 21}},
                {"start": {"line": 0, "character": 24}, "end": {"line": 0, "character": 30}}
            ]
        }]);
        let limits = SemanticResultLimits {
            total: 1,
            per_file: 1,
            per_symbol: 1,
        };
        let translator = Arc::new(translator);
        let item = serde_json::to_value(item).unwrap();
        let mut wire = BufReader::new(&mut server.write_stdout);
        let first_task = {
            let translator = Arc::clone(&translator);
            let item = item.clone();
            tokio::spawn(async move { translator.handle_outgoing_calls(item, limits, None).await })
        };
        let request = read_framed_message(&mut wire).await;
        write_response(
            &mut server.read_half_stdin,
            &request["id"],
            response.clone(),
        )
        .await;
        let first = first_task.await.unwrap().unwrap();
        assert_eq!(first.total_calls, 1);
        assert_eq!(first.total_call_sites, 2);
        assert_eq!(first.returned_call_sites, 1);
        assert_eq!(first.remaining_calls, 1);
        assert_eq!(first.remaining_call_sites, 1);
        let cursor = first.next_cursor.unwrap();

        let second_task = {
            let translator = Arc::clone(&translator);
            tokio::spawn(async move {
                translator
                    .handle_outgoing_calls(item, limits, Some(cursor))
                    .await
            })
        };
        let request = read_framed_message(&mut wire).await;
        write_response(&mut server.read_half_stdin, &request["id"], response).await;
        let second = second_task.await.unwrap().unwrap();
        assert_eq!(second.total_call_sites, 2);
        assert_eq!(second.returned_call_sites, 1);
        assert_eq!(second.remaining_calls, 0);
        assert_eq!(second.remaining_call_sites, 0);
        assert!(second.next_cursor.is_none());
    }

    /// Per the LSP spec, an outgoing call's `fromRanges` are ranges within
    /// the *queried* item's own document, not the callee's (`call.to.uri`) --
    /// the inverse directional convention from incoming calls, tested above.
    #[tokio::test]
    async fn test_handle_outgoing_calls_from_ranges_convert_against_queried_uri() {
        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from("rust");
        let caps = lsp_types::ServerCapabilities {
            call_hierarchy_provider: Some(lsp_types::CallHierarchyServerCapability::Simple(true)),
            ..Default::default()
        };
        let (translator, mut server) = translator_with_capabilities_and_encoding(
            &dir,
            &server_id,
            caps,
            lsp_types::PositionEncodingKind::UTF8,
        );

        let queried_path = dir.path().join("queried.rs");
        fs::write(&queried_path, "aöb").unwrap();
        let queried_uri = Url::from_file_path(&queried_path).unwrap().to_string();

        let callee_path = dir.path().join("callee.rs");
        fs::write(&callee_path, "abc").unwrap();
        let callee_uri = Url::from_file_path(&callee_path).unwrap().to_string();

        let item = call_item("queried_fn", queried_uri);

        let translator = Arc::new(translator);
        let handle = {
            let translator = Arc::clone(&translator);
            let item = serde_json::to_value(item).unwrap();
            tokio::spawn(async move {
                translator
                    .handle_outgoing_calls(item, SemanticResultLimits::default(), None)
                    .await
            })
        };

        let mut wire = BufReader::new(&mut server.write_stdout);
        let request = read_framed_message(&mut wire).await;
        assert_eq!(request["method"], "callHierarchy/outgoingCalls");

        write_response(
            &mut server.read_half_stdin,
            &request["id"],
            serde_json::json!([{
                "to": {
                    "name": "callee_fn",
                    "kind": 12,
                    "uri": callee_uri,
                    "range": {
                        "start": {"line": 0, "character": 0},
                        "end": {"line": 0, "character": 1}
                    },
                    "selectionRange": {
                        "start": {"line": 0, "character": 0},
                        "end": {"line": 0, "character": 1}
                    }
                },
                "fromRanges": [{
                    "start": {"line": 0, "character": 0},
                    "end": {"line": 0, "character": 3}
                }]
            }]),
        )
        .await;

        let result = timeout(Duration::from_secs(2), handle)
            .await
            .expect("handler call should not hang")
            .unwrap()
            .unwrap();

        assert_eq!(result.calls.len(), 1);
        assert_source_available(&result.calls[0].call_sites[0].source);
        let from_range = &result.calls[0].call_sites[0].range;
        assert_eq!(
            from_range.end.character, 3,
            "fromRanges must convert against the queried item's own file (\"aöb\"), not the \
             callee's (\"abc\") -- a byte offset of 3 is UTF-16 column 3 in the former, 4 in \
             the latter"
        );
    }
}
