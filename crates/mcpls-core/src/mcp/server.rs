//! MCP server implementation using rmcp.
//!
//! This module provides the MCP server that exposes LSP capabilities
//! as MCP tools using the rmcp SDK.

use std::marker::PhantomData;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

#[cfg(test)]
use crate::bridge::DocumentSymbolOptions;
use rmcp::handler::server::tool::{
    InputResponses as ToolInputResponses, IntoCallToolResult, RequestState, ToolCallContext,
};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CacheScope, CallToolResponse, CallToolResult, ContentBlock, ElicitRequest, ElicitRequestParams,
    ElicitationSchema, Implementation, InputRequest, InputRequests, InputRequiredResult,
    InputResponses, JsonObject, ListResourcesResult, ListToolsResult, MetaObject,
    PaginatedRequestParams, ProtocolVersion, ReadResourceRequestParams, ReadResourceResponse,
    ReadResourceResult, Resource, ResourceContents, ResourceUpdatedNotificationParam,
    ServerCapabilities, ServerInfo, SubscribeRequestParams, SubscriptionFilter, Tool,
    UnsubscribeRequestParams,
};
use rmcp::service::SubscriptionContext;
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
#[cfg(test)]
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use super::handlers::{APPROVAL_INPUT_ID, HandlerContext, MutationApprovalState};
use super::session::{
    DeferredResource, SessionResource, applied_edit_result_resource_uri,
    edit_approval_resource_uri, edit_diff_resource_uri, event_resource_uris,
    parse_session_resource_uri, project_event_resource_uri, project_events_resource_uri,
    project_status_resource_uri,
};
use super::tools::{
    CachedDiagnosticsParams, CallHierarchyCallsParams, CallHierarchyPrepareParams,
    CodeActionApplyParams, CodeActionListParams, CodeActionPreviewParams, CodeActionsParams,
    CompletionsParams, DaemonStatusParams, DefinitionParams, DiagnosticsMode, DiagnosticsParams,
    DocumentSymbolsParams, FormatDocumentParams, FormatPreviewParams, FormatRange,
    GoToImplementationParams, GoToTypeDefinitionParams, HoverParams, InlayHintsParams,
    InspectSymbolBatchParams, InspectSymbolParams, LEXICAL_PAGE_BYTES, LexicalSearchParams,
    MoveInlineModulePreviewParams, MoveItemPreviewParams, PathRenamePreviewParams,
    ProjectAddParams, ProjectCargoFeaturesParams, ProjectIdParams, ProjectListParams,
    ProjectLspCapabilitiesParams, RangeFormatPreviewParams, ReferencesParams, RenameParams,
    RenamePreviewParams, SemanticPositionParams, SemanticResourceReadParams,
    SemanticResourceReadResult, ServerLogsParams, ServerMessagesParams, SignatureHelpParams,
    StructuralReplacePreviewParams, SubscriptionListParams, WorkspaceEditApplyParams,
    WorkspaceEditApplyResult, WorkspaceEditContention, WorkspaceEditContentionScope,
    WorkspaceEditPreviewParams, WorkspaceEditProviderSynchronization, WorkspaceEditRetry,
    WorkspaceEditRetryAction, WorkspaceSymbolBatchParams, WorkspaceSymbolParams,
};
#[cfg(test)]
use crate::bridge::Translator;
use crate::bridge::lexical::{LexicalSearchBatchRequest, find_matches, validate_path_globs};
use crate::bridge::resources::make_source_uri;
use crate::bridge::resources::make_uri;
#[cfg(test)]
use crate::bridge::translator::DiagnosticOptions;
use crate::bridge::{
    DeferredResourceReference, LexicalSearchRequest, PositionEncoding, ResourceSubscriptions,
    SemanticDiscoveryKind, SymbolHandle, WorkspaceSymbolBatchRequest,
};
use crate::edit_paths::FileOperation;
use crate::edit_plan::EditPlanApprovalSummary;
use crate::edit_plan::PlanId;
use crate::edit_preview::PreviewArtifact;
use crate::project::{
    AppliedEditPlan, ApplyEditPlanOutcome, CanonicalRoot, EditConflict, EditNotReady,
    GeneratedEditRequest, GitRepositoryIdentity, PathRenamePreview, PathRenameRequest,
    ProjectEvent, ProjectEventRecord, ProjectEventSnapshot, ProjectHandle, ProjectId,
    ProjectIdentity, ProjectQueuePressure, ProjectRegistry, ProjectServerCapability, ProjectState,
    ProjectStatusCounts, ProjectStatusSummary, StructuralDialect, StructuralMatchedFile,
    StructuralPreview, StructuralReplaceRequest,
};
use crate::transport::{SessionManagerHandle, TransportSnapshot};

const MAX_SEMANTIC_RESOURCE_RESULT_BYTES: usize = 16 * 1024;

fn deferred_resource_page(
    deferred: &DeferredResource,
    uri: String,
    value: serde_json::Value,
    snapshot_hash: &str,
) -> Result<SemanticResourceReadResult, McpError> {
    let json = serde_json::to_string(&value)
        .map_err(|error| McpError::internal_error(error.to_string(), None))?;
    let total_bytes = json.len();
    if deferred.offset_bytes > total_bytes || !json.is_char_boundary(deferred.offset_bytes) {
        return Err(McpError::invalid_params(
            "invalid deferred resource offset; restart from the original URI",
            None,
        ));
    }

    let mut end = (deferred.offset_bytes + MAX_SEMANTIC_RESOURCE_RESULT_BYTES / 2).min(total_bytes);
    while end > deferred.offset_bytes && !json.is_char_boundary(end) {
        end -= 1;
    }
    loop {
        let next_uri = (end < total_bytes)
            .then(|| format!("mcpls-deferred:///{}?offset_bytes={end}", deferred.token));
        let result = SemanticResourceReadResult {
            uri: uri.clone(),
            mime_type: "application/json".to_owned(),
            text: json[deferred.offset_bytes..end].to_owned(),
            next_uri,
            total_bytes: Some(total_bytes),
            offset_bytes: Some(deferred.offset_bytes),
            returned_bytes: Some(end - deferred.offset_bytes),
            remaining_bytes: Some(total_bytes - end),
            snapshot_hash: Some(snapshot_hash.to_owned()),
        };
        let result_bytes = serde_json::to_vec(&result)
            .map_err(|error| McpError::internal_error(error.to_string(), None))?;
        if result_bytes.len() <= MAX_SEMANTIC_RESOURCE_RESULT_BYTES {
            return Ok(result);
        }
        let next_end = deferred.offset_bytes + (end - deferred.offset_bytes) / 2;
        if next_end == deferred.offset_bytes {
            return Err(McpError::internal_error(
                "deferred resource metadata exceeds the response budget",
                None,
            ));
        }
        end = next_end;
        while !json.is_char_boundary(end) {
            end -= 1;
        }
    }
}

fn edit_diff_resource_page(
    project_id: &ProjectId,
    plan_id: &PlanId,
    offset_bytes: usize,
    diff: &str,
) -> Result<SemanticResourceReadResult, McpError> {
    if offset_bytes > diff.len() || !diff.is_char_boundary(offset_bytes) {
        return Err(McpError::invalid_params(
            "invalid edit diff offset; restart from the original URI",
            None,
        ));
    }

    let total_bytes = diff.len();
    let snapshot_hash = format!("{:x}", Sha256::digest(diff.as_bytes()));
    let mut end = (offset_bytes + MAX_SEMANTIC_RESOURCE_RESULT_BYTES / 2).min(total_bytes);
    while end > offset_bytes && !diff.is_char_boundary(end) {
        end -= 1;
    }
    loop {
        let uri = edit_diff_resource_uri(project_id, plan_id, offset_bytes);
        let next_uri =
            (end < total_bytes).then(|| edit_diff_resource_uri(project_id, plan_id, end));
        let result = SemanticResourceReadResult {
            uri,
            mime_type: "text/x-diff".to_owned(),
            text: diff[offset_bytes..end].to_owned(),
            next_uri,
            total_bytes: Some(total_bytes),
            offset_bytes: Some(offset_bytes),
            returned_bytes: Some(end - offset_bytes),
            remaining_bytes: Some(total_bytes - end),
            snapshot_hash: Some(snapshot_hash.clone()),
        };
        if serde_json::to_vec(&result)
            .map_err(|error| McpError::internal_error(error.to_string(), None))?
            .len()
            <= MAX_SEMANTIC_RESOURCE_RESULT_BYTES
        {
            return Ok(result);
        }
        let next_end = offset_bytes + (end - offset_bytes) / 2;
        if next_end == offset_bytes {
            return Err(McpError::internal_error(
                "edit diff resource metadata exceeds the response budget",
                None,
            ));
        }
        end = next_end;
        while !diff.is_char_boundary(end) {
            end -= 1;
        }
    }
}

fn applied_edit_result_resource_page(
    project_id: &ProjectId,
    plan_id: &PlanId,
    offset_bytes: usize,
    detail: &str,
) -> Result<SemanticResourceReadResult, McpError> {
    if offset_bytes > detail.len() || !detail.is_char_boundary(offset_bytes) {
        return Err(McpError::invalid_params(
            "invalid applied edit result offset; restart from the original URI",
            None,
        ));
    }

    let total_bytes = detail.len();
    let snapshot_hash = format!("{:x}", Sha256::digest(detail.as_bytes()));
    let mut end = (offset_bytes + MAX_SEMANTIC_RESOURCE_RESULT_BYTES / 2).min(total_bytes);
    while end > offset_bytes && !detail.is_char_boundary(end) {
        end -= 1;
    }
    loop {
        let uri = applied_edit_result_resource_uri(project_id, plan_id, offset_bytes);
        let next_uri =
            (end < total_bytes).then(|| applied_edit_result_resource_uri(project_id, plan_id, end));
        let result = SemanticResourceReadResult {
            uri,
            mime_type: "application/json".to_owned(),
            text: detail[offset_bytes..end].to_owned(),
            next_uri,
            total_bytes: Some(total_bytes),
            offset_bytes: Some(offset_bytes),
            returned_bytes: Some(end - offset_bytes),
            remaining_bytes: Some(total_bytes - end),
            snapshot_hash: Some(snapshot_hash.clone()),
        };
        if serde_json::to_vec(&result)
            .map_err(|error| McpError::internal_error(error.to_string(), None))?
            .len()
            <= MAX_SEMANTIC_RESOURCE_RESULT_BYTES
        {
            return Ok(result);
        }
        let next_end = offset_bytes + (end - offset_bytes) / 2;
        if next_end == offset_bytes {
            return Err(McpError::internal_error(
                "applied edit result resource metadata exceeds the response budget",
                None,
            ));
        }
        end = next_end;
        while !detail.is_char_boundary(end) {
            end -= 1;
        }
    }
}

#[derive(Debug, schemars::JsonSchema)]
#[allow(dead_code)] // Schema-only marker for dynamically assembled object responses.
struct StructuredObject {
    #[schemars(flatten)]
    fields: std::collections::BTreeMap<String, serde_json::Value>,
}

fn parse_project_id(value: String) -> Result<ProjectId, McpError> {
    ProjectId::new(value).map_err(|error| McpError::invalid_params(error.to_string(), None))
}

fn parse_position_encoding(value: Option<&str>) -> Result<PositionEncoding, McpError> {
    value.map_or(Ok(PositionEncoding::Utf8), |value| {
        PositionEncoding::from_lsp(value).ok_or_else(|| {
            McpError::invalid_params(format!("unsupported position encoding: {value}"), None)
        })
    })
}

fn parse_structural_dialect(value: &str) -> Result<StructuralDialect, McpError> {
    match value {
        "rust_analyzer_ssr" => Ok(StructuralDialect::RustAnalyzerSsr),
        "ast_grep" => Ok(StructuralDialect::AstGrep),
        _ => Err(McpError::invalid_params(
            format!(
                "unsupported structural dialect: {value}; expected rust_analyzer_ssr or ast_grep"
            ),
            None,
        )),
    }
}

/// MCP-native structured output with a concise legacy text representation.
#[derive(Debug)]
struct Json<T> {
    value: serde_json::Value,
    legacy: String,
    summary: Option<String>,
    marker: PhantomData<T>,
}

impl<T> Deref for Json<T> {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        &self.legacy
    }
}

impl<T> Json<T> {
    const fn new(value: serde_json::Value, legacy: String) -> Self {
        Self {
            value,
            legacy,
            summary: None,
            marker: PhantomData,
        }
    }

    const fn with_summary(value: serde_json::Value, legacy: String, summary: String) -> Self {
        Self {
            value,
            legacy,
            summary: Some(summary),
            marker: PhantomData,
        }
    }

    fn into_result(self, legacy_text: bool) -> CallToolResult {
        let text = self.summary.clone().unwrap_or_else(|| {
            if legacy_text {
                self.legacy.clone()
            } else {
                "Structured result available in structuredContent.".to_owned()
            }
        });
        let mut result = CallToolResult::success(vec![ContentBlock::text(text)]);
        result.structured_content = Some(self.value);
        result
    }
}

impl<T: schemars::JsonSchema + 'static> IntoCallToolResult for Json<T> {
    fn into_call_tool_result(self) -> Result<CallToolResponse, McpError> {
        Ok(self
            .into_result(std::env::var_os("MCPLS_LEGACY_TEXT_RESULTS").is_some())
            .into())
    }
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
struct ToolErrorData {
    code: &'static str,
    message: String,
    action: &'static str,
    retryable: bool,
}

fn operation_error(error: impl std::fmt::Display) -> McpError {
    let error = ToolErrorData {
        code: "operation_failed",
        message: error.to_string(),
        action: "Check project status and request parameters, then retry.",
        retryable: true,
    };
    McpError::internal_error(error.message.clone(), serde_json::to_value(error).ok())
}

fn project_not_registered_error(message: String) -> McpError {
    McpError::invalid_params(
        message.clone(),
        Some(serde_json::json!({
            "code": "project_not_registered",
            "message": message,
            "action": "Call project_list to discover registered IDs; use project_add for a new root.",
            "retryable": false,
        })),
    )
}

fn project_routing_error(error: impl std::fmt::Display) -> McpError {
    let message = error.to_string();
    if message.starts_with("project is not registered:") {
        return project_not_registered_error(message);
    }
    McpError::invalid_params(message, None)
}

fn project_operation_error(error: impl std::fmt::Display) -> McpError {
    let message = error.to_string();
    if message.starts_with("project is not registered:") {
        return project_not_registered_error(message);
    }
    operation_error(message)
}

fn encode_json<T: Serialize>(value: &T) -> Result<Json<StructuredObject>, McpError> {
    let value = serde_json::to_value(value)
        .map_err(|error| McpError::internal_error(error.to_string(), None))?;
    Ok(Json::new(value.clone(), value.to_string()))
}

fn encode_tool_result<T, E>(result: Result<T, E>) -> Result<Json<T>, McpError>
where
    T: Serialize + schemars::JsonSchema,
    E: std::fmt::Display,
{
    result.map_or_else(
        |error| Err(operation_error(error)),
        |value| {
            let structured = serde_json::to_value(&value)
                .map_err(|error| McpError::internal_error(error.to_string(), None))?;
            Ok(Json::new(structured.clone(), structured.to_string()))
        },
    )
}

fn validate_workspace_symbol_batch(params: &WorkspaceSymbolBatchParams) -> Result<(), McpError> {
    if params.queries.is_empty() || params.queries.len() > 32 {
        return Err(McpError::invalid_params(
            "queries must contain between 1 and 32 entries",
            None,
        ));
    }
    if params
        .queries
        .iter()
        .any(|query| query.is_empty() || query.len() > 1_000)
    {
        return Err(McpError::invalid_params(
            "each query must contain between 1 and 1000 bytes",
            None,
        ));
    }
    if params.max_items == 0 || params.max_items > 1_000 {
        return Err(McpError::invalid_params(
            "max_items must be between 1 and 1000",
            None,
        ));
    }
    if !(4_096..=1_048_576).contains(&params.max_bytes) {
        return Err(McpError::invalid_params(
            "max_bytes must be between 4096 and 1048576",
            None,
        ));
    }
    let identity_bytes = params.queries.iter().map(String::len).sum::<usize>()
        + params.queries.len().saturating_mul(128)
        + 256;
    if identity_bytes > params.max_bytes {
        return Err(McpError::invalid_params(
            "max_bytes is too small to return every query identity",
            None,
        ));
    }
    Ok(())
}

fn validate_inspect_symbol_batch(params: &InspectSymbolBatchParams) -> Result<(), McpError> {
    if params.page_token.is_some() {
        if !params.targets.is_empty() {
            return Err(McpError::invalid_params(
                "targets must be empty when page_token is supplied",
                None,
            ));
        }
        return Ok(());
    }
    if params.targets.is_empty()
        || params.targets.len() > crate::bridge::translator::INSPECT_SYMBOL_BATCH_MAX_TARGETS
    {
        return Err(McpError::invalid_params(
            "targets must contain between 1 and 16 symbols",
            None,
        ));
    }
    if params.targets.iter().any(|target| {
        target.symbol_handle.is_none()
            && target
                .query
                .as_ref()
                .is_none_or(|query| query.trim().is_empty())
    }) {
        return Err(McpError::invalid_params(
            "every target requires query or symbol_handle",
            None,
        ));
    }
    if params.targets.iter().any(|target| {
        serde_json::to_vec(target).map_or(usize::MAX, |encoded| encoded.len()) > 4_096
    }) {
        return Err(McpError::invalid_params(
            "each target identity must fit within 4096 serialized bytes",
            None,
        ));
    }
    if params.candidate_limit == 0 || params.candidate_limit > 100 {
        return Err(McpError::invalid_params(
            "candidate_limit must be between 1 and 100",
            None,
        ));
    }
    if params.budget.max_items < params.targets.len() {
        return Err(McpError::invalid_params(
            "budget.max_items must allow at least one item per target",
            None,
        ));
    }
    let identity_bytes = serde_json::to_vec(&params.targets)
        .map_err(|error| McpError::invalid_params(error.to_string(), None))?
        .len();
    let minimum_bytes = identity_bytes
        + crate::bridge::translator::INSPECT_SYMBOL_BATCH_RESPONSE_OVERHEAD_BYTES
        + params.targets.len()
            * crate::bridge::translator::INSPECT_SYMBOL_BATCH_MIN_BYTES_PER_TARGET;
    if params.budget.max_bytes < minimum_bytes || params.budget.max_bytes > 1024 * 1024 {
        return Err(McpError::invalid_params(
            format!(
                "budget.max_bytes must be between {minimum_bytes} and 1048576 for these targets"
            ),
            None,
        ));
    }
    Ok(())
}

#[cfg(test)]
fn bounded_lexical_page(
    matches: Vec<crate::bridge::lexical::LexicalSearchMatch>,
    offset: usize,
    has_next_page: bool,
    max_bytes: usize,
) -> Result<crate::bridge::lexical::LexicalSearchResult, usize> {
    let total_matches = offset
        .saturating_add(matches.len())
        .saturating_add(usize::from(has_next_page));
    bounded_lexical_page_with_accounting(
        matches,
        offset,
        has_next_page,
        max_bytes,
        total_matches,
        0,
        0,
        None,
        "test-snapshot",
    )
}

fn bounded_lexical_page_with_accounting(
    mut matches: Vec<crate::bridge::lexical::LexicalSearchMatch>,
    offset: usize,
    has_next_page: bool,
    max_bytes: usize,
    total_matches: usize,
    scanned_files: usize,
    scanned_bytes: usize,
    cursor_token: Option<&str>,
    snapshot_identity: &str,
) -> Result<crate::bridge::lexical::LexicalSearchResult, usize> {
    let candidate_matches = matches.len();
    loop {
        let truncated = has_next_page || matches.len() < candidate_matches;
        let page = crate::bridge::lexical::LexicalSearchResult {
            returned: matches.len(),
            total: total_matches,
            remaining: total_matches.saturating_sub(offset.saturating_add(matches.len())),
            scanned_files,
            scanned_bytes,
            max_bytes,
            snapshot_identity: snapshot_identity.to_owned(),
            next_cursor: truncated.then(|| {
                cursor_token.map_or_else(
                    || offset.saturating_add(matches.len()).to_string(),
                    |token| crate::project::lexical_page_cursor(token, offset + matches.len()),
                )
            }),
            truncated,
            matches,
        };
        let encoded_bytes = serde_json::to_vec(&page).map_or(usize::MAX, |encoded| encoded.len());
        if encoded_bytes <= max_bytes || page.matches.is_empty() {
            return Ok(page);
        }
        matches = page.matches;
        if let Some(match_with_context) = matches
            .iter_mut()
            .rev()
            .find(|entry| entry.source.is_some())
        {
            match_with_context.source = None;
        } else if matches.len() == 1 {
            return Err(encoded_bytes);
        } else {
            matches.pop();
        }
    }
}

fn bounded_lexical_batch(
    mut batch: crate::bridge::lexical::LexicalSearchBatchResult,
    max_bytes: usize,
) -> Result<crate::bridge::lexical::LexicalSearchBatchResult, usize> {
    loop {
        batch.returned = batch
            .entries
            .iter()
            .filter_map(|entry| entry.result.as_ref())
            .map(|result| result.returned)
            .sum();
        let encoded_bytes = serde_json::to_vec(&batch).map_or(usize::MAX, |encoded| encoded.len());
        if encoded_bytes <= max_bytes {
            return Ok(batch);
        }
        let Some(entry) = batch.entries.iter_mut().rev().find(|entry| {
            entry
                .result
                .as_ref()
                .is_some_and(|result| !result.matches.is_empty())
        }) else {
            return Err(encoded_bytes);
        };
        let result = entry.result.as_mut().expect("entry was selected above");
        result.matches.pop();
        result.returned = result.matches.len();
        result.truncated = true;
        batch.truncated = true;
    }
}

const fn effective_lexical_page_bytes(requested: usize) -> usize {
    if requested < LEXICAL_PAGE_BYTES {
        requested
    } else {
        LEXICAL_PAGE_BYTES
    }
}

fn call_hierarchy_item_path(item: &serde_json::Value) -> Result<PathBuf, McpError> {
    let uri = item
        .get("uri")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| McpError::invalid_params("call hierarchy item is missing uri", None))?
        .parse::<lsp_types::Uri>()
        .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
    crate::bridge::uri_to_path(&uri).ok_or_else(|| {
        McpError::invalid_params("call hierarchy item uri must be an absolute file URI", None)
    })
}

#[derive(Serialize, schemars::JsonSchema)]
struct ActorGroupState {
    group_id: usize,
    roots: Vec<PathBuf>,
}

#[derive(Serialize, schemars::JsonSchema)]
struct ProjectLspCapabilitiesResponse {
    project_id: String,
    servers: Vec<ProjectServerCapability>,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
struct DaemonPersistenceSnapshot {
    configured: bool,
    last_error: Option<String>,
}

#[derive(Debug, Clone)]
struct DaemonSnapshot {
    project_counts: ProjectStatusCounts,
    actor_groups: usize,
    project_summaries: Vec<ProjectStatusSummary>,
    persistence: DaemonPersistenceSnapshot,
    transport: TransportSnapshot,
    session_count: usize,
    queue_pressure: ProjectQueuePressure,
    shutting_down: bool,
}

impl DaemonSnapshot {
    const fn lifecycle(&self) -> &'static str {
        if self.shutting_down {
            "shutting_down"
        } else {
            "running"
        }
    }
}

fn actor_group_states(actor_group_roots: Vec<Vec<PathBuf>>) -> Vec<ActorGroupState> {
    actor_group_roots
        .into_iter()
        .enumerate()
        .map(|(group_id, roots)| ActorGroupState { group_id, roots })
        .collect()
}

fn project_state_json(
    identity: &ProjectIdentity,
    state: &ProjectState,
    actor_groups: &[ActorGroupState],
) -> serde_json::Value {
    serde_json::json!({
        "project_id": identity.id().as_str(),
        "root": identity.root().as_path(),
        "roots": project_root_paths(identity),
        "repository_root": identity.repository_identity().map(GitRepositoryIdentity::common_dir),
        "status": state.status().as_str(),
        "last_error": state.last_error(),
        "dormancy": state.dormancy().map(|dormancy| serde_json::json!({
            "reason": dormancy.reason().as_str(),
            "idle_for_ms": dormancy
                .idle_for()
                .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)),
        })),
        "configured_language_servers": state.runtime().configured_language_ids(),
        "active_language_servers": state.runtime().active_language_ids(),
        "open_document_count": state.open_document_count(),
        "generation": state.runtime().generation(),
        "actor_group_count": actor_groups.len(),
        "actor_groups": actor_groups,
    })
}

fn project_root_paths(identity: &ProjectIdentity) -> Vec<PathBuf> {
    identity
        .roots()
        .iter()
        .map(CanonicalRoot::as_path)
        .map(Path::to_path_buf)
        .collect()
}

fn project_status_counts_json(counts: ProjectStatusCounts) -> serde_json::Value {
    serde_json::json!({
        "starting": counts.starting,
        "ready": counts.ready,
        "degraded": counts.degraded,
        "restarting": counts.restarting,
        "dormant": counts.dormant,
        "stopping": counts.stopping,
        "stopped": counts.stopped,
        "failed": counts.failed,
    })
}

fn project_status_summaries_json(summaries: &[ProjectStatusSummary]) -> serde_json::Value {
    summaries
        .iter()
        .map(|summary| {
            serde_json::json!({
                "project_id": summary.project_id.as_str(),
                "status": summary.status.as_str(),
                "actor_group_count": summary.actor_group_count,
                "roots": summary.roots,
            })
        })
        .collect::<Vec<_>>()
        .into()
}

fn project_queue_pressure_json(pressure: ProjectQueuePressure) -> serde_json::Value {
    serde_json::json!({
        "queued": pressure.queued,
        "capacity": pressure.capacity,
    })
}

#[derive(Debug, Clone, Copy)]
enum DaemonHealth {
    Healthy,
    Degraded,
    Failed,
}

impl DaemonHealth {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::Degraded => "degraded",
            Self::Failed => "failed",
        }
    }
}

const fn health_status(snapshot: &DaemonSnapshot) -> DaemonHealth {
    if snapshot.project_counts.failed > 0 {
        DaemonHealth::Failed
    } else if snapshot.persistence.last_error.is_some()
        || snapshot.project_counts.degraded > 0
        || snapshot.project_counts.restarting > 0
        || snapshot.project_counts.stopping > 0
    {
        DaemonHealth::Degraded
    } else {
        DaemonHealth::Healthy
    }
}

#[derive(Serialize, schemars::JsonSchema)]
struct SubscriptionListResult {
    subscriptions: Vec<String>,
}

fn project_events_json(
    project_id: &ProjectId,
    snapshot: &ProjectEventSnapshot,
) -> serde_json::Value {
    serde_json::json!({
        "project_id": project_id.as_str(),
        "next_cursor": snapshot.next_sequence(),
        "next_uri": snapshot.truncated().then(|| {
            format!("mcpls-project-events:///{project_id}?since={}", snapshot.next_sequence())
        }),
        "resync_required": snapshot.resync_required(),
        "truncated": snapshot.truncated(),
        "retention_floor": snapshot.retention_floor(),
        "returned_events": snapshot.events().len(),
        "first_sequence": snapshot.first_sequence(),
        "last_sequence": snapshot.last_sequence(),
        "events": snapshot
            .events()
            .iter()
            .map(|record| project_event_json(project_id, record))
            .collect::<Vec<_>>(),
    })
}

fn project_event_json(project_id: &ProjectId, record: &ProjectEventRecord) -> serde_json::Value {
    let event = record.event().json_value();
    let total_bytes = serde_json::to_vec(&event).map_or(0, |bytes| bytes.len());
    if total_bytes <= MAX_INLINE_PROJECT_EVENT_BYTES {
        return record.json_value();
    }
    serde_json::json!({
        "sequence": record.sequence(),
        "event": {"kind": event.get("kind").cloned().unwrap_or(serde_json::Value::Null)},
        "resource": {
            "uri": project_event_resource_uri(project_id, record.sequence()),
            "total_bytes": total_bytes,
        },
    })
}

fn project_relative_paths(roots: &[PathBuf], paths: Vec<PathBuf>) -> Vec<String> {
    paths
        .into_iter()
        .map(|path| {
            roots
                .iter()
                .find_map(|root| path.strip_prefix(root).ok())
                .map_or_else(
                    || "<outside-project>".to_owned(),
                    |relative| relative.display().to_string(),
                )
        })
        .collect()
}

const MAX_INLINE_APPLIED_ITEMS: usize = 8;

fn workspace_edit_apply_result(
    outcome: ApplyEditPlanOutcome,
    project_id: &str,
    roots: &[PathBuf],
) -> WorkspaceEditApplyResult {
    match outcome {
        ApplyEditPlanOutcome::Applied(AppliedEditPlan {
            plan_id,
            operations,
            unified_diff,
            complete_unified_diff: _,
            committed_files,
            verification,
            provider_synchronization,
        }) => {
            let committed_files = project_relative_paths(roots, committed_files);
            let committed_file_count = committed_files.len();
            let operation_count = operations.len();
            let provider_synchronization_count = provider_synchronization.len();
            let details_truncated = unified_diff.len() > MAX_INLINE_APPLIED_DIFF_BYTES
                || committed_file_count > MAX_INLINE_APPLIED_ITEMS
                || operation_count > MAX_INLINE_APPLIED_ITEMS
                || provider_synchronization_count > MAX_INLINE_APPLIED_ITEMS
                || committed_files
                    .iter()
                    .any(|path| path.len() > MAX_APPROVAL_TEXT_BYTES)
                || operations
                    .iter()
                    .any(|operation| operation.len() > MAX_APPROVAL_TEXT_BYTES)
                || provider_synchronization.iter().any(|provider| {
                    provider.provider.len() > MAX_APPROVAL_TEXT_BYTES
                        || provider
                            .message
                            .as_ref()
                            .is_some_and(|message| message.len() > MAX_APPROVAL_TEXT_BYTES)
                });
            let detail_resource = details_truncated.then(|| {
                applied_edit_result_resource_uri(
                    &ProjectId::new(project_id.to_owned()).expect("registered project id"),
                    &plan_id,
                    0,
                )
            });
            let unified_diff =
                crate::util::truncate_str(&unified_diff, MAX_INLINE_APPLIED_DIFF_BYTES);
            let semantic_state = (!provider_synchronization.is_empty()).then(|| {
                if provider_synchronization
                    .iter()
                    .all(|provider| provider.synchronized)
                {
                    "synchronized".to_owned()
                } else {
                    "degraded".to_owned()
                }
            });
            let provider_synchronization = provider_synchronization
                .into_iter()
                .take(MAX_INLINE_APPLIED_ITEMS)
                .map(|provider| WorkspaceEditProviderSynchronization {
                    provider: bounded_approval_text(&provider.provider),
                    synchronized: provider.synchronized,
                    watched_file_notifications: provider.watched_file_notifications,
                    message: provider
                        .message
                        .map(|message| bounded_approval_text(&message)),
                })
                .collect();
            WorkspaceEditApplyResult::Applied {
                project_id: project_id.to_owned(),
                plan_id: plan_id.as_str().to_owned(),
                committed: true,
                committed_files: committed_files
                    .into_iter()
                    .take(MAX_INLINE_APPLIED_ITEMS)
                    .map(|path| bounded_approval_text(&path))
                    .collect(),
                committed_file_count,
                operations: operations
                    .into_iter()
                    .take(MAX_INLINE_APPLIED_ITEMS)
                    .map(|operation| bounded_approval_text(&operation))
                    .collect(),
                operation_count,
                unified_diff,
                detail_resource,
                verification: verification.map(|status| status.as_str().to_owned()),
                provider_synchronization,
                provider_synchronization_count,
                details_truncated,
                semantic_state,
            }
        }
        ApplyEditPlanOutcome::NotReady(EditNotReady {
            plan_id,
            blocked_paths,
            retry_after_ms,
        }) => {
            let blocked_path_count = blocked_paths.len();
            let blocked_paths = project_relative_paths(roots, blocked_paths)
                .into_iter()
                .take(MAX_APPROVAL_ITEMS)
                .collect();
            WorkspaceEditApplyResult::NotReady {
                reason: "edit_in_progress".to_owned(),
                project_id: project_id.to_owned(),
                plan_id: plan_id.as_str().to_owned(),
                committed: false,
                retry: WorkspaceEditRetry {
                    action: WorkspaceEditRetryAction::RetryApply,
                    same_plan: true,
                    after_ms: Some(retry_after_ms),
                },
                contention: WorkspaceEditContention {
                    scope: WorkspaceEditContentionScope::SameWorktree,
                    blocked_paths,
                    blocked_path_count,
                },
            }
        }
        ApplyEditPlanOutcome::Conflict(EditConflict {
            plan_id,
            changed_paths,
            reason,
        }) => WorkspaceEditApplyResult::Conflict {
            reason,
            project_id: project_id.to_owned(),
            plan_id: plan_id.as_str().to_owned(),
            committed: false,
            retry: WorkspaceEditRetry {
                action: WorkspaceEditRetryAction::PreviewAgain,
                same_plan: false,
                after_ms: None,
            },
            changed_paths: project_relative_paths(roots, changed_paths),
        },
    }
}

fn workspace_edit_apply_json(
    outcome: ApplyEditPlanOutcome,
    project_id: &str,
    roots: &[PathBuf],
) -> Result<Json<WorkspaceEditApplyResult>, McpError> {
    let result = workspace_edit_apply_result(outcome, project_id, roots);
    let value = serde_json::to_value(&result)
        .map_err(|error| McpError::internal_error(error.to_string(), None))?;
    // Keep the legacy text channel machine-readable for clients that still
    // parse tool text, while structuredContent carries the typed result for
    // negotiated MCP clients.
    let legacy = serde_json::to_string(&result)
        .map_err(|error| McpError::internal_error(error.to_string(), None))?;
    let summary = match &result {
        WorkspaceEditApplyResult::Applied {
            plan_id,
            committed_files,
            ..
        } => format!(
            "Applied edit plan {plan_id}; committed {} file(s).",
            committed_files.len()
        ),
        WorkspaceEditApplyResult::NotReady {
            plan_id,
            contention,
            retry,
            ..
        } => format!(
            "Edit plan {plan_id} is not ready: {} path(s) are in use. Retry the same plan after about {} ms.",
            contention.blocked_path_count,
            retry.after_ms.unwrap_or(100)
        ),
        WorkspaceEditApplyResult::Conflict { plan_id, .. } => {
            format!("Edit plan {plan_id} is stale; preview the change again before retrying.")
        }
    };
    Ok(Json::with_summary(value, legacy, summary))
}

const MAX_APPROVAL_ITEMS: usize = 64;
const MAX_APPROVAL_TEXT_BYTES: usize = 256;
const APPROVAL_METHOD: &str = "tools/call";

fn approval_arguments_digest(project_id: &str, plan_id: &PlanId) -> String {
    let input = format!("{project_id}\0{}", plan_id.as_str());
    format!("{:x}", Sha256::digest(input.as_bytes()))
}

fn approval_binding(
    session_id: &str,
    principal: Option<&str>,
    tool_name: &str,
    arguments_digest: &str,
) -> Vec<u8> {
    format!(
        "mcpls-mrtr-v1\0{APPROVAL_METHOD}\0{tool_name}\0{session_id}\0{}\0{arguments_digest}",
        principal.unwrap_or_default()
    )
    .into_bytes()
}

fn bounded_approval_text(value: &str) -> String {
    crate::util::truncate_str(value, MAX_APPROVAL_TEXT_BYTES)
}

fn approval_summary_json(summary: &EditPlanApprovalSummary) -> serde_json::Value {
    let mut created_files = Vec::new();
    let mut renamed_files = Vec::new();
    let mut deleted_files = Vec::new();
    let mut risk_flags = Vec::new();
    for operation in summary.file_operations.iter().take(MAX_APPROVAL_ITEMS) {
        match operation {
            FileOperation::Create { path, .. } => {
                created_files.push(bounded_approval_text(&path.display().to_string()));
            }
            FileOperation::Rename { from, to, .. } => {
                renamed_files.push(serde_json::json!({
                    "from": bounded_approval_text(&from.display().to_string()),
                    "to": bounded_approval_text(&to.display().to_string()),
                }));
            }
            FileOperation::Delete { path, .. } => {
                deleted_files.push(bounded_approval_text(&path.display().to_string()));
            }
        }
    }
    if !created_files.is_empty() {
        risk_flags.push("creates_files");
    }
    if !renamed_files.is_empty() {
        risk_flags.push("renames_files");
    }
    if !deleted_files.is_empty() {
        risk_flags.push("deletes_files");
    }
    if summary.diff_truncated {
        risk_flags.push("diff_truncated");
    }
    if !summary.safe_to_apply {
        risk_flags.push("plan_not_safe_to_apply");
    }
    let diff_files = summary
        .diff_files
        .iter()
        .take(MAX_APPROVAL_ITEMS)
        .map(|file| {
            serde_json::json!({
                "path": bounded_approval_text(&file.path().display().to_string()),
                "additions": file.additions(),
                "deletions": file.deletions(),
            })
        })
        .collect::<Vec<_>>();
    let operation_kind = if !deleted_files.is_empty() {
        "delete"
    } else if !renamed_files.is_empty() {
        "rename"
    } else if !created_files.is_empty() {
        "create"
    } else {
        "text_edit"
    };
    let project_id = ProjectId::new(summary.project_id.clone()).expect("stored plan project id");
    let details_truncated = summary.affected_files.len() > MAX_APPROVAL_ITEMS
        || summary.operations.len() > MAX_APPROVAL_ITEMS
        || summary.file_operations.len() > MAX_APPROVAL_ITEMS
        || summary.diff_files.len() > MAX_APPROVAL_ITEMS
        || summary
            .affected_files
            .iter()
            .any(|path| path.display().to_string().len() > MAX_APPROVAL_TEXT_BYTES)
        || summary
            .operations
            .iter()
            .any(|operation| operation.len() > MAX_APPROVAL_TEXT_BYTES);
    serde_json::json!({
        "plan_id": summary.plan_id.as_str(),
        "project_id": summary.project_id,
        "operation_kind": operation_kind,
        "affected_file_count": summary.affected_files.len(),
        "operation_count": summary.operations.len(),
        "file_operation_count": summary.file_operations.len(),
        "diff_file_count": summary.diff_files.len(),
        "snapshot_count": summary.snapshot_hashes.len(),
        "affected_files": summary
            .affected_files
            .iter()
            .take(MAX_APPROVAL_ITEMS)
            .map(|path| bounded_approval_text(&path.display().to_string()))
            .collect::<Vec<_>>(),
        "created_files": created_files,
        "renamed_files": renamed_files,
        "deleted_files": deleted_files,
        "operations": summary
            .operations
            .iter()
            .take(MAX_APPROVAL_ITEMS)
            .map(|operation| bounded_approval_text(operation))
            .collect::<Vec<_>>(),
        "diff_files": diff_files,
        "diff_truncated": summary.diff_truncated,
        "safe_to_apply": summary.safe_to_apply,
        "risk_flags": risk_flags,
        "details_truncated": details_truncated,
        "detail_resource": {
            "uri": edit_approval_resource_uri(&project_id, &summary.plan_id, 0),
            "mime_type": "application/json",
        },
    })
}

fn edit_approval_resource_page(
    project_id: &ProjectId,
    plan_id: &PlanId,
    offset_bytes: usize,
    detail: &str,
) -> Result<SemanticResourceReadResult, McpError> {
    let mut page = applied_edit_result_resource_page(project_id, plan_id, offset_bytes, detail)?;
    page.uri = edit_approval_resource_uri(project_id, plan_id, offset_bytes);
    page.next_uri = page.next_uri.as_ref().and_then(|uri| {
        super::session::parse_applied_edit_result_resource_uri(uri)
            .map(|(_, _, offset)| edit_approval_resource_uri(project_id, plan_id, offset))
    });
    Ok(page)
}

fn approval_detail_json(summary: &EditPlanApprovalSummary) -> serde_json::Value {
    serde_json::json!({
        "plan_id": summary.plan_id.as_str(),
        "project_id": summary.project_id,
        "affected_files": summary.affected_files.iter().map(|path| path.display().to_string()).collect::<Vec<_>>(),
        "operations": summary.operations,
        "file_operations": summary.file_operations.iter().map(|operation| match operation {
            FileOperation::Create { path, .. } => serde_json::json!({"kind": "create", "path": path}),
            FileOperation::Rename { from, to, .. } => serde_json::json!({"kind": "rename", "from": from, "to": to}),
            FileOperation::Delete { path, recursive } => serde_json::json!({"kind": "delete", "path": path, "recursive": recursive}),
        }).collect::<Vec<_>>(),
        "diff_files": summary.diff_files.iter().map(|file| serde_json::json!({"path": file.path(), "additions": file.additions(), "deletions": file.deletions()})).collect::<Vec<_>>(),
        "diff_truncated": summary.diff_truncated,
        "safe_to_apply": summary.safe_to_apply,
        "snapshot_hashes": summary.snapshot_hashes,
        "versions": summary.versions,
    })
}

fn approval_input_required(
    sealed_state: String,
    summary: &EditPlanApprovalSummary,
) -> Result<CallToolResponse, McpError> {
    let summary_json = approval_summary_json(summary);
    let schema = serde_json::from_value::<ElicitationSchema>(serde_json::json!({
        "type": "object",
        "properties": {
            "approved": {
                "type": "boolean",
                "description": "Apply this exact previewed edit plan"
            }
        },
        "required": ["approved"],
        "additionalProperties": false
    }))
    .map_err(|error| McpError::internal_error(error.to_string(), None))?;
    let operation_kind = summary_json["operation_kind"].as_str().unwrap_or("edit");
    let affected_file_count = summary_json["affected_file_count"]
        .as_u64()
        .unwrap_or_default();
    let mut input_requests = InputRequests::new();
    input_requests.insert(
        APPROVAL_INPUT_ID.to_owned(),
        InputRequest::Elicitation(ElicitRequest::new(
            ElicitRequestParams::FormElicitationParams {
                meta: None,
                message: format!(
                    "Approve {operation_kind} affecting {affected_file_count} workspace file(s)?"
                ),
                requested_schema: schema,
            },
        )),
    );
    let mut meta = MetaObject::new();
    meta.0.insert("approvalSummary".to_owned(), summary_json);
    Ok(
        InputRequiredResult::new(Some(input_requests), Some(sealed_state))
            .with_meta(meta)
            .into(),
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ApprovalDecision {
    Accept,
    Decline,
}

struct ApprovalRequest<'a> {
    id: &'a ProjectId,
    plan_id: &'a PlanId,
    tool_name: &'a str,
    binding: &'a [u8],
    arguments_digest: String,
}

fn parse_approval_response(
    input_responses: Option<InputResponses>,
) -> Result<ApprovalDecision, McpError> {
    let Some(input_responses) = input_responses else {
        return Err(McpError::invalid_params(
            "approval response is required with requestState",
            None,
        ));
    };
    if input_responses.len() != 1 || !input_responses.contains_key(APPROVAL_INPUT_ID) {
        return Err(McpError::invalid_params(
            "approval response must contain exactly one approval entry",
            None,
        ));
    }
    let response = &input_responses[APPROVAL_INPUT_ID];
    match response.get("action").and_then(serde_json::Value::as_str) {
        Some("accept") => {
            if response
                .get("content")
                .and_then(|content| content.get("approved"))
                != Some(&serde_json::Value::Bool(true))
            {
                return Err(McpError::invalid_params(
                    "approval response must explicitly set approved=true",
                    None,
                ));
            }
            Ok(ApprovalDecision::Accept)
        }
        Some("decline" | "cancel") => Ok(ApprovalDecision::Decline),
        _ => Err(McpError::invalid_params(
            "approval response action must be accept, decline, or cancel",
            None,
        )),
    }
}

fn preview_artifact_json(result: &PreviewArtifact, project_id: &str) -> serde_json::Value {
    let preconditions = result
        .plan
        .files()
        .iter()
        .map(|file| {
            serde_json::json!({
                "path": file.path(),
                "source": match file.source() {
                    crate::edit_plan::SnapshotSource::Disk => "disk",
                    crate::edit_plan::SnapshotSource::OpenDocument => "open_document",
                },
                "version": file.version(),
                "sha256": file.content_hash(),
            })
        })
        .collect::<Vec<_>>();
    let diff_files = result
        .plan
        .diff_files()
        .iter()
        .map(|file| {
            serde_json::json!({
                "path": file.path(),
                "additions": file.additions(),
                "deletions": file.deletions(),
            })
        })
        .collect::<Vec<_>>();
    let mut value = serde_json::json!({
        "project_id": project_id,
        "plan_id": result.plan.id().as_str(),
        "unified_diff": result.plan.unified_diff(),
        "diff_files": diff_files,
        "diff_truncated": result.plan.diff_truncated(),
        "affected_files": result.affected_files,
        "operations": result.plan.operations(),
        "preconditions": preconditions,
        "conflicts": result.conflicts,
        "unsupported": result.unsupported,
        "safe_to_apply": result.plan.safe_to_apply(),
    });
    if let Some(verification) = result.verification {
        value["verification"] = serde_json::json!(verification.as_str());
    }
    if let Some(producer) = result.producer {
        value["producer"] = serde_json::json!(producer.as_str());
    }
    if result.plan.diff_truncated() {
        value["diff_resource"] = serde_json::json!({
            "uri": format!(
                "mcpls-edit-diff:///{project_id}?plan_id={}&offset_bytes=0",
                result.plan.id().as_str(),
            ),
            "mime_type": "text/x-diff",
        });
    }
    value
}

fn structural_preview_json(result: &StructuralPreview, project_id: &str) -> serde_json::Value {
    let mut matched_files = result
        .matches
        .iter()
        .map(|matched| matched.path.clone())
        .collect::<Vec<_>>();
    matched_files.sort();
    matched_files.dedup();
    let matches = result
        .matches
        .iter()
        .map(|matched| {
            serde_json::json!({
                "path": matched.path,
                "range": matched.range,
            })
        })
        .collect::<Vec<_>>();
    let mut value = result.artifact.as_ref().map_or_else(
        || {
            serde_json::json!({
                "project_id": project_id,
                "safe_to_apply": false,
            })
        },
        |artifact| preview_artifact_json(artifact, project_id),
    );
    value["engine"] = serde_json::json!(result.dialect.engine());
    value["dialect"] = serde_json::json!(result.dialect.as_str());
    value["semantic_confidence"] = serde_json::json!(match result.dialect {
        StructuralDialect::RustAnalyzerSsr => "semantic",
        StructuralDialect::AstGrep => "structural",
    });
    value["parse_only"] = serde_json::json!(result.parse_only);
    value["match_count"] = serde_json::json!(matches.len());
    value["returned_match_count"] = serde_json::json!(matches.len());
    value["remaining_match_count"] = serde_json::json!(0);
    value["matched_file_count"] = serde_json::json!(matched_files.len());
    value["matched_files"] = serde_json::json!(matched_files);
    value["matches"] = serde_json::json!(matches);
    if result.artifact.is_none() {
        value["unsupported"] = serde_json::json!(Vec::<String>::new());
    }
    value
}

fn structural_source_resource(
    file: &StructuralMatchedFile,
) -> Result<DeferredResourceReference, McpError> {
    Ok(DeferredResourceReference {
        uri: make_source_uri(
            &file.path,
            1,
            1,
            file.total_lines,
            1,
            &file.content_hash,
            file.document_version,
        )
        .map_err(|error| McpError::internal_error(error.to_string(), None))?,
        kind: "source_context".to_owned(),
        snapshot_hash: file.content_hash.clone(),
        document_version: file.document_version,
        total_bytes: Some(file.total_bytes),
    })
}

fn structural_match_inventory_json(
    result: &StructuralPreview,
) -> Result<serde_json::Value, McpError> {
    let files = result
        .matched_files
        .iter()
        .enumerate()
        .map(|(file, matched)| {
            Ok(serde_json::json!({
                "file": file,
                "path": matched.path,
                "snapshot_hash": matched.content_hash,
                "document_version": matched.document_version,
                "source_resource": structural_source_resource(matched)?,
            }))
        })
        .collect::<Result<Vec<_>, McpError>>()?;
    let file_indexes = result
        .matched_files
        .iter()
        .enumerate()
        .map(|(index, file)| (file.path.as_path(), index))
        .collect::<std::collections::HashMap<_, _>>();
    let matches = result
        .matches
        .iter()
        .map(|matched| {
            let file = file_indexes.get(matched.path.as_path()).ok_or_else(|| {
                McpError::internal_error("structural match has no source snapshot", None)
            })?;
            Ok(serde_json::json!({
                "file": file,
                "range": [
                    matched.range.start.line,
                    matched.range.start.character,
                    matched.range.end.line,
                    matched.range.end.character,
                ],
            }))
        })
        .collect::<Result<Vec<_>, McpError>>()?;
    Ok(serde_json::json!({
        "file_count": files.len(),
        "match_count": matches.len(),
        "files": files,
        "matches": matches,
    }))
}

fn bounded_structural_preview_json(
    registry: &ProjectRegistry,
    project_id: &ProjectId,
    result: &StructuralPreview,
) -> Result<serde_json::Value, McpError> {
    let value = structural_preview_json(result, project_id.as_str());
    if serde_json::to_vec(&value)
        .map_err(|error| McpError::internal_error(error.to_string(), None))?
        .len()
        <= MAX_SEMANTIC_RESOURCE_RESULT_BYTES
    {
        return Ok(value);
    }

    let matches_resource = registry
        .store_deferred_resource(
            project_id,
            "structural_match_inventory",
            structural_match_inventory_json(result)?,
        )
        .map_err(|error| McpError::internal_error(error, None))?;
    let mut compact = serde_json::json!({
        "project_id": project_id.as_str(),
        "engine": result.dialect.engine(),
        "dialect": result.dialect.as_str(),
        "semantic_confidence": match result.dialect {
            StructuralDialect::RustAnalyzerSsr => "semantic",
            StructuralDialect::AstGrep => "structural",
        },
        "parse_only": result.parse_only,
        "match_count": result.matches.len(),
        "returned_match_count": 0,
        "remaining_match_count": result.matches.len(),
        "matched_file_count": result.matched_files.len(),
        "matched_files": Vec::<String>::new(),
        "matches": Vec::<serde_json::Value>::new(),
        "matches_resource": matches_resource,
        "safe_to_apply": false,
        "unsupported": Vec::<String>::new(),
    });
    if let Some(artifact) = &result.artifact {
        let details = preview_artifact_json(artifact, project_id.as_str());
        let details_resource = registry
            .store_deferred_resource(project_id, "structural_plan_details", details)
            .map_err(|error| McpError::internal_error(error, None))?;
        compact["plan_id"] = serde_json::json!(artifact.plan.id().as_str());
        compact["safe_to_apply"] = serde_json::json!(artifact.plan.safe_to_apply());
        compact["affected_file_count"] = serde_json::json!(artifact.affected_files.len());
        compact["operation_count"] = serde_json::json!(artifact.plan.operations().len());
        compact["precondition_count"] = serde_json::json!(artifact.plan.files().len());
        compact["diff_file_count"] = serde_json::json!(artifact.plan.diff_files().len());
        compact["conflict_count"] = serde_json::json!(artifact.conflicts.len());
        compact["unsupported_count"] = serde_json::json!(artifact.unsupported.len());
        compact["diff_truncated"] = serde_json::json!(artifact.plan.diff_truncated());
        compact["plan_details_resource"] = serde_json::json!(details_resource);
        if let Some(verification) = artifact.verification {
            compact["verification"] = serde_json::json!(verification.as_str());
        }
        if let Some(producer) = artifact.producer {
            compact["producer"] = serde_json::json!(producer.as_str());
        }
    }
    debug_assert!(
        serde_json::to_vec(&compact)
            .is_ok_and(|json| json.len() <= MAX_SEMANTIC_RESOURCE_RESULT_BYTES)
    );
    Ok(compact)
}

fn path_rename_preview_json(result: &PathRenamePreview, project_id: &str) -> serde_json::Value {
    let mut value = preview_artifact_json(&result.artifact, project_id);
    value["semantic_providers"] = serde_json::json!(result.providers);
    value["semantic_provider_available"] = serde_json::json!(!result.providers.is_empty());
    value["semantic_edit_count"] = serde_json::json!(result.semantic_edit_count);
    value
}

const ADVERTISED_TOOL_PAGE_SIZE: usize = 12;
const PROJECT_LIST_PAGE_SIZE: usize = 32;
const RESOURCE_PAGE_SIZE: usize = 64;
const PROJECT_EVENT_PAGE_SIZE: usize = 64;
const MAX_INLINE_PROJECT_EVENT_BYTES: usize = 128;
const MAX_INLINE_APPLIED_DIFF_BYTES: usize = 4 * 1024;
#[cfg(test)]
const NATIVE_TOOL_CATALOG_MAX_BYTES: usize = 48 * 1024;
const LEGACY_COMPATIBILITY_TOOLS: &[&str] = &[
    "rename_symbol",
    "format_document",
    "get_code_actions",
    "get_cached_diagnostics",
    "workspace_symbol_search_batch",
    "inspect_symbol_batch",
    "range_format_preview",
];
const DEFAULT_TOOL_PAGE: &[&str] = &[
    "workspace_symbol_search",
    "inspect_symbol",
    "get_diagnostics",
    "format_preview",
    "lexical_search",
    "read_semantic_resource",
    "structural_replace_preview",
    "workspace_edit_preview",
    "workspace_edit_apply",
    "code_action_apply",
    "project_list",
];

#[derive(Debug, Serialize, JsonSchema)]
#[serde(untagged)]
enum WorkspaceSymbolSearchResponse {
    One(Box<crate::bridge::WorkspaceSymbolResult>),
    Many(Box<crate::bridge::WorkspaceSymbolBatchResult>),
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(untagged)]
enum InspectSymbolResponse {
    One(Box<crate::bridge::InspectSymbolResult>),
    Many(Box<crate::bridge::InspectSymbolBatchResult>),
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(untagged)]
enum LexicalSearchResponse {
    One(Box<crate::bridge::lexical::LexicalSearchResult>),
    Many(Box<crate::bridge::lexical::LexicalSearchBatchResult>),
}

/// Remove JSON Schema presentation metadata while retaining every invocation contract.
fn compact_advertised_input_schema(schema: &mut JsonObject) {
    for key in [
        "$schema",
        "deprecated",
        "description",
        "examples",
        "readOnly",
        "title",
        "writeOnly",
    ] {
        schema.remove(key);
    }

    for (key, value) in schema {
        if matches!(key.as_str(), "properties" | "$defs" | "definitions") {
            if let Value::Object(entries) = value {
                for schema in entries.values_mut() {
                    compact_advertised_schema_value(schema);
                }
            }
        } else {
            compact_advertised_schema_value(value);
        }
    }
}

fn compact_advertised_schema_value(value: &mut Value) {
    match value {
        Value::Object(schema) => compact_advertised_input_schema(schema),
        Value::Array(values) => {
            for value in values {
                compact_advertised_schema_value(value);
            }
        }
        _ => {}
    }
}

fn compact_advertised_description(name: &str) -> String {
    format!("MCPLS {}.", name.replace('_', " "))
}

#[cfg(test)]
fn native_catalog_bytes(tools: &[Tool]) -> usize {
    let instructions = include_str!("server_instructions.txt").trim_end().len();
    tools
        .iter()
        .map(|tool| {
            serde_json::to_vec(tool)
                .unwrap()
                .len()
                .saturating_add(instructions)
        })
        .sum()
}

fn advertised_tools() -> Vec<Tool> {
    let mut tools = McplsServer::tool_router()
        .list_all()
        .into_iter()
        .filter(|tool| !LEGACY_COMPATIBILITY_TOOLS.contains(&tool.name.as_ref()))
        .map(|mut tool| {
            compact_advertised_input_schema(Arc::make_mut(&mut tool.input_schema));
            tool.description = Some(compact_advertised_description(tool.name.as_ref()).into());
            tool.output_schema = None;
            tool
        })
        .collect::<Vec<_>>();
    tools.sort_by(|left, right| {
        tool_catalog_rank(left.name.as_ref()).cmp(&tool_catalog_rank(right.name.as_ref()))
    });
    tools
}

fn tool_catalog_rank(name: &str) -> (usize, &str) {
    (
        DEFAULT_TOOL_PAGE
            .iter()
            .position(|candidate| *candidate == name)
            .unwrap_or(DEFAULT_TOOL_PAGE.len()),
        name,
    )
}

fn advertised_tools_page(cursor: Option<&str>) -> Result<(Vec<Tool>, Option<String>), String> {
    let tools = advertised_tools();
    let start = match cursor {
        Some(cursor) => cursor
            .parse::<usize>()
            .map_err(|_| format!("invalid tools/list cursor: {cursor}"))?,
        None => 0,
    };
    if start >= tools.len() {
        return Err(format!("tools/list cursor is outside the catalog: {start}"));
    }

    let end = start
        .saturating_add(ADVERTISED_TOOL_PAGE_SIZE)
        .min(tools.len());
    Ok((
        tools[start..end].to_vec(),
        (end < tools.len()).then(|| end.to_string()),
    ))
}

fn resource_page(
    resources: Vec<Resource>,
    cursor: Option<&str>,
) -> Result<(Vec<Resource>, Option<String>), String> {
    let start = match cursor {
        Some(cursor) => cursor
            .parse::<usize>()
            .map_err(|_| format!("invalid resources/list cursor: {cursor}"))?,
        None => 0,
    };
    if cursor.is_some() && start >= resources.len() {
        return Err(format!(
            "resources/list cursor is outside the catalog: {start}"
        ));
    }

    let end = start
        .saturating_add(RESOURCE_PAGE_SIZE)
        .min(resources.len());
    let next_cursor = (end < resources.len()).then(|| end.to_string());
    Ok((
        resources
            .into_iter()
            .skip(start)
            .take(end - start)
            .collect(),
        next_cursor,
    ))
}

fn project_list_page(
    project_count: usize,
    cursor: Option<&str>,
) -> Result<(std::ops::Range<usize>, Option<String>), String> {
    let start = match cursor {
        Some(cursor) => cursor
            .parse::<usize>()
            .map_err(|_| format!("invalid project_list cursor: {cursor}"))?,
        None => 0,
    };
    if cursor.is_some() && start >= project_count {
        return Err(format!(
            "project_list cursor is outside the project list: {start}"
        ));
    }

    let end = start
        .saturating_add(PROJECT_LIST_PAGE_SIZE)
        .min(project_count);
    let next_cursor = (end < project_count).then(|| end.to_string());
    Ok((start..end, next_cursor))
}

/// MCP server that exposes LSP capabilities as tools.
pub struct McplsServer {
    context: Arc<HandlerContext>,
}

enum ListenEvent {
    Event(ProjectId, ProjectEvent),
    Lagged(ProjectId, u64),
}

fn supports_cache_hints(context: &rmcp::service::RequestContext<RoleServer>) -> bool {
    context
        .protocol_version()
        .is_some_and(|version| version >= ProtocolVersion::V_2026_07_28)
}

fn private_resource_result(
    contents: Vec<ResourceContents>,
    supports_cache_hints: bool,
) -> ReadResourceResult {
    let result = ReadResourceResult::new(contents);
    if supports_cache_hints {
        result.with_ttl_ms(0).with_cache_scope(CacheScope::Private)
    } else {
        result
    }
}

async fn send_listen_update(context: &SubscriptionContext, uri: String) -> bool {
    tokio::select! {
        () = context.cancelled() => false,
        result = context.sink().notify_resource_updated(uri) => result.is_ok(),
    }
}

impl Clone for McplsServer {
    fn clone(&self) -> Self {
        self.for_session()
    }
}

#[tool_router]
impl McplsServer {
    async fn semantic_target(
        &self,
        project_id: Option<String>,
        symbol_handle: Option<SymbolHandle>,
        file_path: String,
        line: u32,
        character: u32,
    ) -> Result<(ProjectHandle, String, u32, u32), McpError> {
        if let Some(symbol_handle) = symbol_handle {
            let project_id = project_id.ok_or_else(|| {
                McpError::invalid_params(
                    "project_id is required with symbol_handle".to_owned(),
                    None,
                )
            })?;
            let id = parse_project_id(project_id)?;
            let (actor, target) = self
                .context
                .resolve_symbol_handle(&id, symbol_handle)
                .await
                .map_err(|error| {
                    let message = error;
                    let code = if message.contains("stale_symbol_handle") {
                        "stale_symbol_handle"
                    } else {
                        "invalid_symbol_handle"
                    };
                    McpError::invalid_params(
                        message,
                        Some(serde_json::json!({
                            "code": code,
                            "refresh": "rerun symbol discovery and use the new handle"
                        })),
                    )
                })?;
            return Ok((actor, target.file_path, target.line, target.character));
        }
        if file_path.is_empty() {
            return Err(McpError::invalid_params(
                "file_path, line, and character or project_id and symbol_handle are required"
                    .to_owned(),
                None,
            ));
        }
        let actor = if let Some(project_id) = project_id {
            let id = parse_project_id(project_id)?;
            self.context.required_actor_for_project(&id).await
        } else {
            self.context.required_actor_for_path(&file_path).await
        }
        .map_err(project_routing_error)?;
        Ok((actor, file_path, line, character))
    }

    async fn call_hierarchy_target(
        &self,
        params: CallHierarchyCallsParams,
    ) -> Result<
        (
            ProjectHandle,
            serde_json::Value,
            crate::bridge::SemanticResultLimits,
        ),
        McpError,
    > {
        let limits = params.limits;
        if params.symbol_handle.is_some() {
            let (actor, file_path, line, character) = self
                .semantic_target(params.project_id, params.symbol_handle, String::new(), 0, 0)
                .await?;
            let item = actor
                .prepare_call_hierarchy(file_path, line, character, None)
                .await
                .map_err(|error| McpError::invalid_params(error.to_string(), None))?
                .items
                .into_iter()
                .next()
                .ok_or_else(|| {
                    McpError::invalid_params(
                        "symbol handle does not resolve to a callable item".to_owned(),
                        None,
                    )
                })?;
            return serde_json::to_value(item)
                .map(|item| (actor, item, limits))
                .map_err(|error| McpError::internal_error(error.to_string(), None));
        }
        let item = params.item.ok_or_else(|| {
            McpError::invalid_params("item or project_id and symbol_handle are required", None)
        })?;
        let path = call_hierarchy_item_path(&item)?;
        let actor = self
            .context
            .required_actor_for_path(&path)
            .await
            .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
        Ok((actor, item, limits))
    }
    async fn daemon_snapshot(&self) -> DaemonSnapshot {
        let projects = self.context.project_registry.status_snapshot().await;
        DaemonSnapshot {
            project_counts: projects.counts,
            actor_groups: projects.actor_groups,
            project_summaries: projects.summaries,
            persistence: DaemonPersistenceSnapshot {
                configured: self.context.project_registry.persistence_configured(),
                last_error: self.context.project_registry.persistence_error().await,
            },
            transport: (*self.context.transport).clone(),
            session_count: self.context.session_count().await,
            queue_pressure: projects.queue_pressure,
            shutting_down: self.context.project_registry.is_shutting_down(),
        }
    }

    async fn project_state_json(
        &self,
        project_id: &ProjectId,
        identity: &ProjectIdentity,
        state: &ProjectState,
    ) -> Result<Json<StructuredObject>, McpError> {
        let actor_group_roots = self
            .context
            .project_registry
            .actor_group_roots(project_id)
            .await
            .map_err(|error| McpError::internal_error(error.to_string(), None))?;
        let actor_groups = actor_group_states(actor_group_roots);
        let mut value = project_state_json(identity, state, &actor_groups);
        let cargo_features = self
            .context
            .project_registry
            .cargo_features(project_id)
            .await
            .map_err(|error| McpError::internal_error(error.to_string(), None))?;
        if let Some(object) = value.as_object_mut() {
            object.insert(
                "cargo_features".to_owned(),
                serde_json::to_value(cargo_features)
                    .map_err(|error| McpError::internal_error(error.to_string(), None))?,
            );
        }
        encode_json(&value)
    }

    async fn attach_subscription(
        &self,
        project_id: ProjectId,
        actors: &[ProjectHandle],
        uri: String,
        peer: rmcp::Peer<RoleServer>,
    ) -> Result<(), McpError> {
        self.context
            .subscriptions
            .subscribe(uri.clone())
            .await
            .map_err(|error| McpError::invalid_params(error, None))?;
        self.context
            .event_sink
            .track_subscription(project_id.clone(), uri);
        self.context.event_sink.attach(project_id, actors, peer);
        Ok(())
    }

    async fn attach_project_subscription(
        &self,
        project_id: ProjectId,
        uri: String,
        peer: rmcp::Peer<RoleServer>,
    ) -> Result<(), McpError> {
        let actors = self
            .context
            .project_registry
            .actors_for_project(&project_id)
            .await
            .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
        self.attach_subscription(project_id, &actors, uri, peer)
            .await
    }

    async fn read_project_status_resource(
        &self,
        project_id: ProjectId,
        uri: String,
        supports_cache_hints: bool,
    ) -> Result<ReadResourceResponse, McpError> {
        let identity = self
            .context
            .project_registry
            .identity(&project_id)
            .await
            .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
        let state = self
            .context
            .project_registry
            .status(&project_id)
            .await
            .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
        let json = self
            .project_state_json(&project_id, &identity, &state)
            .await?;
        Ok(private_resource_result(
            vec![ResourceContents::text(json.legacy, uri)],
            supports_cache_hints,
        )
        .into())
    }

    async fn read_project_events_resource(
        &self,
        project_id: ProjectId,
        cursor: Option<u64>,
        uri: String,
        supports_cache_hints: bool,
    ) -> Result<ReadResourceResponse, McpError> {
        let actor = self
            .context
            .project_registry
            .actor_for_project(&project_id)
            .await
            .map_err(project_routing_error)?;
        let snapshot = actor.event_snapshot(cursor, PROJECT_EVENT_PAGE_SIZE);
        let json = encode_json(&project_events_json(&project_id, &snapshot))?;
        Ok(private_resource_result(
            vec![ResourceContents::text(json.legacy, uri)],
            supports_cache_hints,
        )
        .into())
    }

    async fn read_project_event_resource(
        &self,
        project_id: ProjectId,
        sequence: u64,
        uri: String,
        supports_cache_hints: bool,
    ) -> Result<ReadResourceResponse, McpError> {
        let actor = self
            .context
            .project_registry
            .actor_for_project(&project_id)
            .await
            .map_err(project_routing_error)?;
        let record = actor.event_record(sequence).ok_or_else(|| {
            McpError::invalid_params(
                "stale_resource: project event is no longer retained; reread project events",
                None,
            )
        })?;
        let json = serde_json::to_string(&record.json_value())
            .map_err(|error| McpError::internal_error(error.to_string(), None))?;
        Ok(private_resource_result(
            vec![ResourceContents::text(json, uri)],
            supports_cache_hints,
        )
        .into())
    }

    async fn read_edit_diff_resource(
        &self,
        project_id: ProjectId,
        plan_id: PlanId,
        offset_bytes: usize,
        uri: String,
        supports_cache_hints: bool,
    ) -> Result<ReadResourceResponse, McpError> {
        if !self.context.owns_plan(&plan_id).await {
            return Err(McpError::invalid_params(
                "edit plan is not owned by this MCP session",
                None,
            ));
        }
        let actor = self
            .context
            .project_registry
            .actor_for_project(&project_id)
            .await
            .map_err(project_routing_error)?;
        let diff = actor
            .read_edit_plan_diff(plan_id.clone(), project_id.as_str().to_owned())
            .await
            .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
        let page = edit_diff_resource_page(&project_id, &plan_id, offset_bytes, &diff)?;
        let text = serde_json::to_string(&page)
            .map_err(|error| McpError::internal_error(error.to_string(), None))?;
        Ok(private_resource_result(
            vec![ResourceContents::text(text, uri)],
            supports_cache_hints,
        )
        .into())
    }

    async fn read_applied_edit_result_resource(
        &self,
        project_id: ProjectId,
        plan_id: PlanId,
        offset_bytes: usize,
        uri: String,
        supports_cache_hints: bool,
    ) -> Result<ReadResourceResponse, McpError> {
        if !self.context.recognizes_plan(&plan_id).await {
            return Err(McpError::invalid_params(
                "applied edit result is not owned by this MCP session",
                None,
            ));
        }
        let actor = self
            .context
            .project_registry
            .actor_for_project(&project_id)
            .await
            .map_err(project_routing_error)?;
        let detail = actor
            .read_applied_edit_detail(plan_id.clone(), project_id.as_str().to_owned())
            .await
            .map_err(|_| {
                McpError::invalid_params(
                    "stale_resource: applied edit result is no longer retained",
                    None,
                )
            })?;
        let page = applied_edit_result_resource_page(&project_id, &plan_id, offset_bytes, &detail)?;
        let text = serde_json::to_string(&page)
            .map_err(|error| McpError::internal_error(error.to_string(), None))?;
        Ok(private_resource_result(
            vec![ResourceContents::text(text, uri)],
            supports_cache_hints,
        )
        .into())
    }

    async fn read_edit_approval_resource(
        &self,
        project_id: ProjectId,
        plan_id: PlanId,
        offset_bytes: usize,
        uri: String,
        supports_cache_hints: bool,
    ) -> Result<ReadResourceResponse, McpError> {
        if !self.context.owns_plan(&plan_id).await {
            return Err(McpError::invalid_params(
                "edit approval is not owned by this MCP session",
                None,
            ));
        }
        let summary = self
            .context
            .project_registry
            .inspect_edit_plan(&project_id, plan_id.clone())
            .await
            .map_err(|_| {
                McpError::invalid_params(
                    "stale_resource: edit approval is no longer retained",
                    None,
                )
            })?;
        let detail = serde_json::to_string(&approval_detail_json(&summary))
            .map_err(|error| McpError::internal_error(error.to_string(), None))?;
        let page = edit_approval_resource_page(&project_id, &plan_id, offset_bytes, &detail)?;
        let text = serde_json::to_string(&page)
            .map_err(|error| McpError::internal_error(error.to_string(), None))?;
        Ok(private_resource_result(
            vec![ResourceContents::text(text, uri)],
            supports_cache_hints,
        )
        .into())
    }

    async fn preview_project_edit(
        &self,
        id: &ProjectId,
        edit: lsp_types::WorkspaceEdit,
        encoding: PositionEncoding,
    ) -> Result<Json<StructuredObject>, McpError> {
        let artifact = self
            .context
            .project_registry
            .preview_edit(id, edit, encoding)
            .await
            .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
        self.context.remember_plan(artifact.plan.id().clone()).await;
        encode_json(&preview_artifact_json(&artifact, id.as_str()))
    }

    async fn preview_generated_edit(
        &self,
        id: &ProjectId,
        request: GeneratedEditRequest,
        encoding: PositionEncoding,
    ) -> Result<Json<StructuredObject>, McpError> {
        let result = self
            .context
            .project_registry
            .preview_generated_edit(id, request, encoding)
            .await
            .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
        let Some(artifact) = result.artifact else {
            return encode_json(&serde_json::json!({
                "project_id": id.as_str(),
                "supported": result.supported,
                "changed": false,
            }));
        };
        self.context.remember_plan(artifact.plan.id().clone()).await;
        encode_json(&preview_artifact_json(&artifact, id.as_str()))
    }

    async fn preview_generated_supported_edit(
        &self,
        id: &ProjectId,
        request: GeneratedEditRequest,
        encoding: PositionEncoding,
    ) -> Result<Json<StructuredObject>, McpError> {
        let result = self
            .context
            .project_registry
            .preview_generated_edit(id, request, encoding)
            .await
            .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
        let Some(artifact) = result.artifact else {
            return encode_json(&serde_json::json!({
                "project_id": id.as_str(),
                "supported": result.supported,
                "changed": false,
            }));
        };
        self.context.remember_plan(artifact.plan.id().clone()).await;
        let mut value = preview_artifact_json(&artifact, id.as_str());
        value["supported"] = serde_json::Value::Bool(result.supported);
        value["changed"] = serde_json::Value::Bool(true);
        encode_json(&value)
    }

    async fn semantic_discovery(
        &self,
        params: SemanticPositionParams,
        kind: SemanticDiscoveryKind,
    ) -> Result<Json<crate::bridge::SemanticDiscoveryResult>, McpError> {
        let (actor, file_path, line, character) = self
            .semantic_target(
                Some(params.project_id),
                params.symbol_handle,
                params.file_path,
                params.line,
                params.character,
            )
            .await?;
        let result = actor
            .semantic_discovery(file_path, line, character, kind)
            .await
            .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
        encode_tool_result::<_, std::convert::Infallible>(Ok(result))
    }

    #[cfg(test)]
    async fn apply_project_plan(
        &self,
        id: &ProjectId,
        plan_id: PlanId,
        wait_timeout_ms: Option<u64>,
    ) -> Result<Json<WorkspaceEditApplyResult>, McpError> {
        if !self.context.claim_plan(&plan_id).await {
            return Err(McpError::invalid_params(
                "edit plan is not owned by this MCP session",
                None,
            ));
        }
        self.apply_project_plan_claimed(id, plan_id, wait_timeout_ms)
            .await
    }

    async fn apply_project_plan_claimed(
        &self,
        id: &ProjectId,
        plan_id: PlanId,
        wait_timeout_ms: Option<u64>,
    ) -> Result<Json<WorkspaceEditApplyResult>, McpError> {
        let roots = self
            .context
            .project_registry
            .identity(id)
            .await
            .map_err(|error| McpError::invalid_params(error.to_string(), None))?
            .roots()
            .iter()
            .map(|root| root.as_path().to_path_buf())
            .collect::<Vec<_>>();
        let outcome = self
            .context
            .project_registry
            .apply_edit_plan_with_wait(
                id,
                plan_id.clone(),
                Some(self.context.session_id().to_owned()),
                None,
                Duration::from_millis(wait_timeout_ms.unwrap_or(250)),
            )
            .await
            .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
        if matches!(outcome, ApplyEditPlanOutcome::NotReady(_)) {
            self.context.remember_plan(plan_id).await;
        } else {
            self.context.finish_plan(&plan_id).await;
        }
        workspace_edit_apply_json(outcome, id.as_str(), &roots)
    }

    async fn apply_project_plan_with_approval(
        &self,
        tool_name: &str,
        params: WorkspaceEditApplyParams,
        request_state: Option<String>,
        input_responses: Option<InputResponses>,
        cancellation: CancellationToken,
    ) -> Result<CallToolResponse, McpError> {
        if !self.context.supports_mutation_approval() {
            return self
                .apply_project_plan_direct(params, request_state, input_responses, cancellation)
                .await;
        }
        if cancellation.is_cancelled() {
            return Err(McpError::invalid_params(
                "mutating apply was cancelled before approval",
                None,
            ));
        }
        let id = parse_project_id(params.project_id.clone())?;
        let plan_id = PlanId::parse(params.plan_id.clone())
            .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
        let wait_timeout_ms = params.wait_timeout_ms;
        let arguments_digest = approval_arguments_digest(id.as_str(), &plan_id);
        let principal = None;
        let binding = approval_binding(
            self.context.session_id(),
            principal,
            tool_name,
            &arguments_digest,
        );
        match (request_state, input_responses) {
            (None, None) => {
                self.prepare_mutation_approval(&id, &plan_id, tool_name, &binding, arguments_digest)
                    .await
            }
            (Some(sealed), Some(input_responses)) => {
                self.retry_mutation_approval(
                    ApprovalRequest {
                        id: &id,
                        plan_id: &plan_id,
                        tool_name,
                        binding: &binding,
                        arguments_digest,
                    },
                    sealed,
                    input_responses,
                    cancellation,
                    wait_timeout_ms,
                )
                .await
            }
            (None, Some(_)) => Err(McpError::invalid_params(
                "inputResponses requires the matching requestState",
                None,
            )),
            (Some(_), None) => Err(McpError::invalid_params(
                "requestState requires inputResponses",
                None,
            )),
        }
    }

    /// Apply a session-owned plan after a legacy client dispatches the
    /// correctly annotated mutating tool call through its own approval gate.
    async fn apply_project_plan_direct(
        &self,
        params: WorkspaceEditApplyParams,
        request_state: Option<String>,
        input_responses: Option<InputResponses>,
        cancellation: CancellationToken,
    ) -> Result<CallToolResponse, McpError> {
        match (request_state, input_responses) {
            (None, None) => {}
            (None, Some(_)) => {
                return Err(McpError::invalid_params(
                    "inputResponses requires the matching requestState",
                    None,
                ));
            }
            (Some(_), None) => {
                return Err(McpError::invalid_params(
                    "requestState requires inputResponses",
                    None,
                ));
            }
            (Some(_), Some(_)) => {
                return Err(McpError::invalid_params(
                    "this client does not support elicitation retries; call apply without requestState",
                    None,
                ));
            }
        }
        if cancellation.is_cancelled() {
            return Err(McpError::invalid_params(
                "mutating apply was cancelled before filesystem changes",
                None,
            ));
        }
        let WorkspaceEditApplyParams {
            project_id,
            plan_id,
            wait_timeout_ms,
        } = params;
        let id = parse_project_id(project_id)?;
        let plan_id = PlanId::parse(plan_id)
            .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
        if !self.context.claim_plan(&plan_id).await {
            return Err(McpError::invalid_params(
                "edit plan is not owned by this MCP session",
                None,
            ));
        }
        self.apply_project_plan_claimed(&id, plan_id, wait_timeout_ms)
            .await?
            .into_call_tool_result()
    }

    async fn prepare_mutation_approval(
        &self,
        id: &ProjectId,
        plan_id: &PlanId,
        tool_name: &str,
        binding: &[u8],
        arguments_digest: String,
    ) -> Result<CallToolResponse, McpError> {
        if !self.context.owns_plan(plan_id).await {
            return Err(McpError::invalid_params(
                "edit plan is not owned by this MCP session",
                None,
            ));
        }
        let summary = self
            .context
            .project_registry
            .inspect_edit_plan(id, plan_id.clone())
            .await
            .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
        if !summary.safe_to_apply {
            return Err(McpError::invalid_params(
                "edit plan is not safe to apply",
                None,
            ));
        }
        let state = MutationApprovalState {
            session_id: self.context.session_id().to_owned(),
            principal: None,
            method: APPROVAL_METHOD.to_owned(),
            tool_name: tool_name.to_owned(),
            project_id: id.as_str().to_owned(),
            plan_id: plan_id.as_str().to_owned(),
            arguments_digest,
            snapshot_hashes: summary.snapshot_hashes.clone(),
            versions: summary.versions.clone(),
            nonce: PlanId::new().as_str().to_owned(),
        };
        let nonce = state.nonce.clone();
        let sealed = self
            .context
            .seal_approval_state(&state, binding)
            .map_err(|error| McpError::internal_error(error.to_string(), None))?;
        self.context.remember_approval(nonce).await;
        approval_input_required(sealed, &summary)
    }

    async fn retry_mutation_approval(
        &self,
        request: ApprovalRequest<'_>,
        sealed: String,
        input_responses: InputResponses,
        cancellation: CancellationToken,
        wait_timeout_ms: Option<u64>,
    ) -> Result<CallToolResponse, McpError> {
        let state = self
            .context
            .open_approval_state(&sealed, request.binding)
            .map_err(|error| {
                McpError::invalid_params(
                    format!("invalid or expired approval request state: {error}"),
                    None,
                )
            })?;
        if state.session_id != self.context.session_id()
            || state.principal.is_some()
            || state.method != APPROVAL_METHOD
            || state.tool_name != request.tool_name
            || state.project_id != request.id.as_str()
            || state.plan_id != request.plan_id.as_str()
            || state.arguments_digest != request.arguments_digest
        {
            return Err(McpError::invalid_params(
                "approval request state does not match this apply request",
                None,
            ));
        }
        if parse_approval_response(Some(input_responses))? == ApprovalDecision::Decline {
            let _ = self.context.consume_approval(&state.nonce).await;
            return Err(McpError::invalid_params(
                "approval declined or cancelled; no files were changed",
                None,
            ));
        }
        match self
            .context
            .project_registry
            .inspect_edit_plan(request.id, request.plan_id.clone())
            .await
        {
            Ok(summary) => {
                if summary.snapshot_hashes != state.snapshot_hashes
                    || summary.versions != state.versions
                {
                    return Err(McpError::invalid_params(
                        "edit plan is stale; preview the change again",
                        None,
                    ));
                }
            }
            Err(error) if error.to_string().contains("edit plan not found") => {
                // A concurrent accepted retry may have claimed the plan. The
                // sealed snapshot summary remains the integrity check, and
                // the registry's in-flight join handles the duplicate apply.
            }
            Err(error) => return Err(McpError::invalid_params(error.to_string(), None)),
        }
        if cancellation.is_cancelled() {
            return Err(McpError::invalid_params(
                "mutating apply was cancelled before filesystem changes",
                None,
            ));
        }
        if !self.context.has_approval(&state.nonce).await {
            return Err(McpError::invalid_params(
                "approval request has already been consumed",
                None,
            ));
        }
        if !self.context.claim_plan(request.plan_id).await {
            return Err(McpError::invalid_params(
                "edit plan is not owned by this MCP session",
                None,
            ));
        }
        let result = self
            .apply_project_plan_claimed(request.id, request.plan_id.clone(), wait_timeout_ms)
            .await?;
        if !matches!(
            result.value.get("status"),
            Some(serde_json::Value::String(status)) if status == "not_ready"
        ) {
            let _ = self.context.consume_approval(&state.nonce).await;
        }
        result.into_call_tool_result()
    }

    #[cfg(test)]
    async fn apply_project_plan_params(
        &self,
        params: WorkspaceEditApplyParams,
    ) -> Result<Json<WorkspaceEditApplyResult>, McpError> {
        let WorkspaceEditApplyParams {
            project_id,
            plan_id,
            wait_timeout_ms,
        } = params;
        let id = parse_project_id(project_id)?;
        let plan_id = PlanId::parse(plan_id)
            .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
        self.apply_project_plan(&id, plan_id, wait_timeout_ms).await
    }

    /// Create a new MCP server with an empty project registry.
    #[must_use]
    pub fn new(subscriptions: Arc<ResourceSubscriptions>) -> Self {
        Self::from_registry(subscriptions, ProjectRegistry::new(32))
    }

    /// Create a server with an explicitly shared project registry.
    #[must_use]
    pub fn new_with_registry(
        subscriptions: Arc<ResourceSubscriptions>,
        project_registry: ProjectRegistry,
    ) -> Self {
        Self::from_registry(subscriptions, project_registry)
    }

    /// Create a server from the shared project registry without a global
    /// mutable translator.
    #[must_use]
    pub fn from_registry(
        subscriptions: Arc<ResourceSubscriptions>,
        project_registry: ProjectRegistry,
    ) -> Self {
        Self {
            context: Arc::new(HandlerContext::from_registry(
                subscriptions,
                project_registry,
            )),
        }
    }

    /// Create a server with explicit daemon transport metadata.
    #[must_use]
    pub(crate) fn from_registry_with_transport(
        subscriptions: Arc<ResourceSubscriptions>,
        project_registry: ProjectRegistry,
        transport: TransportSnapshot,
        session_manager: SessionManagerHandle,
    ) -> Self {
        Self {
            context: Arc::new(HandlerContext::from_registry_with_transport(
                subscriptions,
                project_registry,
                transport,
                session_manager,
            )),
        }
    }

    /// Clone the server for one MCP session while sharing project actors.
    ///
    /// Session-local subscriptions are intentionally not shared with the
    /// source server or any other session.
    #[must_use]
    pub fn for_session(&self) -> Self {
        Self {
            context: Arc::new(self.context.for_session()),
        }
    }

    /// Register a project root for long-lived lifecycle and routing operations.
    #[tool(description = "Register a project root under a stable project ID.")]
    async fn project_add(
        &self,
        Parameters(ProjectAddParams {
            project_id,
            root,
            config,
        }): Parameters<ProjectAddParams>,
    ) -> Result<Json<StructuredObject>, McpError> {
        let id = parse_project_id(project_id)?;
        let config = config
            .map(serde_json::from_value::<crate::config::ProjectConfig>)
            .transpose()
            .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
        let canonical_root = CanonicalRoot::new(&root)
            .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
        let repository = GitRepositoryIdentity::discover(canonical_root.as_path())
            .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
        let identity = repository.map_or_else(
            || ProjectIdentity::new(id.clone(), canonical_root.clone()),
            |repository| {
                ProjectIdentity::new(id.clone(), canonical_root.clone())
                    .with_repository_identity(repository)
            },
        );
        let actor = self
            .context
            .project_registry
            .add_with_config(identity.clone(), config)
            .await
            .map_err(|error| McpError::internal_error(error.to_string(), None))?;
        let identity = self
            .context
            .project_registry
            .identity(&id)
            .await
            .map_err(|error| McpError::internal_error(error.to_string(), None))?;
        let state = actor
            .query()
            .await
            .map_err(|error| McpError::internal_error(error.to_string(), None))?;
        self.project_state_json(&id, &identity, &state).await
    }

    /// Activate a registered project and return while its language servers load.
    #[tool(
        description = "Activate a registered project. Starts its applicable language servers and returns while they load; poll project_status until it is Ready for code intelligence."
    )]
    async fn project_activate(
        &self,
        Parameters(ProjectIdParams { project_id }): Parameters<ProjectIdParams>,
    ) -> Result<Json<StructuredObject>, McpError> {
        let id = parse_project_id(project_id)?;
        let identity = self
            .context
            .project_registry
            .identity(&id)
            .await
            .map_err(|error| McpError::internal_error(error.to_string(), None))?;
        let state = self
            .context
            .project_registry
            .activate(&id)
            .await
            .map_err(|error| McpError::internal_error(error.to_string(), None))?;
        self.project_state_json(&id, &identity, &state).await
    }

    /// List all registered projects without waiting on project actors.
    #[tool(description = "List registered projects and canonical roots in cursor pages.")]
    async fn project_list(
        &self,
        Parameters(ProjectListParams { cursor }): Parameters<ProjectListParams>,
    ) -> Result<Json<StructuredObject>, McpError> {
        let projects = self.context.project_registry.list().await;
        let (page, next_cursor) = project_list_page(projects.len(), cursor.as_deref())
            .map_err(|error| McpError::invalid_params(error, None))?;
        let result: Vec<_> = projects[page]
            .iter()
            .map(|project| {
                serde_json::json!({
                    "project_id": project.id().as_str(),
                    "root": project.root().as_path(),
                    "roots": project_root_paths(project),
                    "repository_root": project.repository_identity().map(GitRepositoryIdentity::common_dir),
                })
            })
            .collect();
        encode_json(&serde_json::json!({
            "projects": result,
            "returned": result.len(),
            "truncated": next_cursor.is_some(),
            "next_cursor": next_cursor,
        }))
    }

    /// List resource subscriptions owned by this MCP session.
    #[tool(description = "List resource URIs subscribed by this MCP session.")]
    async fn subscription_list(
        &self,
        Parameters(_params): Parameters<SubscriptionListParams>,
    ) -> Result<Json<SubscriptionListResult>, McpError> {
        let subscriptions = self.context.subscriptions.sorted_snapshot().await;
        encode_tool_result::<_, std::convert::Infallible>(Ok(SubscriptionListResult {
            subscriptions,
        }))
    }

    /// Return a cheap process and project liveness snapshot.
    #[tool(description = "Return daemon liveness and non-blocking project lifecycle counts.")]
    async fn health(
        &self,
        Parameters(_params): Parameters<DaemonStatusParams>,
    ) -> Result<Json<StructuredObject>, McpError> {
        let snapshot = self.daemon_snapshot().await;
        encode_json(&serde_json::json!({
            "status": health_status(&snapshot).as_str(),
            "lifecycle": snapshot.lifecycle(),
            "persistence": snapshot.persistence,
            "transport": snapshot.transport,
            "session_count": snapshot.session_count,
            "queue_pressure": project_queue_pressure_json(snapshot.queue_pressure),
            "projects": project_status_counts_json(snapshot.project_counts),
            "actor_groups": snapshot.actor_groups,
            "project_summaries": project_status_summaries_json(&snapshot.project_summaries),
        }))
    }

    /// Return daemon version, uptime, and a cheap project status snapshot.
    #[tool(description = "Return daemon version, uptime, and non-blocking project status.")]
    async fn server_status(
        &self,
        Parameters(_params): Parameters<DaemonStatusParams>,
    ) -> Result<Json<StructuredObject>, McpError> {
        let snapshot = self.daemon_snapshot().await;
        encode_json(&serde_json::json!({
            "version": env!("CARGO_PKG_VERSION"),
            "uptime_seconds": self.context.started_at.elapsed().as_secs(),
            "lifecycle": snapshot.lifecycle(),
            "persistence": snapshot.persistence,
            "transport": snapshot.transport,
            "session_count": snapshot.session_count,
            "queue_pressure": project_queue_pressure_json(snapshot.queue_pressure),
            "projects": project_status_counts_json(snapshot.project_counts),
            "actor_groups": snapshot.actor_groups,
            "project_summaries": project_status_summaries_json(&snapshot.project_summaries),
        }))
    }

    /// Return the current state for one registered project.
    #[tool(description = "Return lifecycle status and the last failure for a project.")]
    async fn project_status(
        &self,
        Parameters(ProjectIdParams { project_id }): Parameters<ProjectIdParams>,
    ) -> Result<Json<StructuredObject>, McpError> {
        let id = parse_project_id(project_id)?;
        let identity = self
            .context
            .project_registry
            .identity(&id)
            .await
            .map_err(|error| McpError::internal_error(error.to_string(), None))?;
        let state = self
            .context
            .project_registry
            .status(&id)
            .await
            .map_err(|error| McpError::internal_error(error.to_string(), None))?;
        self.project_state_json(&id, &identity, &state).await
    }

    /// Remove a project and shut down its actor.
    #[tool(
        description = "Permanently forget a registered project and stop its actor. Do not use for session cleanup or activation recovery."
    )]
    async fn project_remove(
        &self,
        Parameters(ProjectIdParams { project_id }): Parameters<ProjectIdParams>,
    ) -> Result<Json<StructuredObject>, McpError> {
        let id = parse_project_id(project_id)?;
        self.context
            .project_registry
            .remove(id.clone())
            .await
            .map_err(|error| McpError::internal_error(error.to_string(), None))?;
        encode_json(&serde_json::json!({
            "project_id": id.as_str(),
            "removed": true,
        }))
    }

    /// Preview an LSP `WorkspaceEdit` without changing any files.
    #[tool(
        description = "Preview a project-scoped LSP WorkspaceEdit. The returned plan is owned by this MCP session and includes a plan ID, unified diff, affected files, preconditions, conflicts, unsupported operations, and explicit safety state."
    )]
    async fn workspace_edit_preview(
        &self,
        Parameters(WorkspaceEditPreviewParams {
            project_id,
            workspace_edit,
            position_encoding,
        }): Parameters<WorkspaceEditPreviewParams>,
    ) -> Result<Json<StructuredObject>, McpError> {
        let id = parse_project_id(project_id)?;
        let edit: lsp_types::WorkspaceEdit = serde_json::from_value(workspace_edit)
            .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
        let encoding = parse_position_encoding(position_encoding.as_deref())?;
        self.preview_project_edit(&id, edit, encoding).await
    }

    /// Request a rename from the project LSP and preview the resulting edit.
    #[tool(
        description = "Preview an LSP rename as a session-owned workspace edit plan. Apply the returned plan from this MCP session with workspace_edit_apply."
    )]
    async fn rename_preview(
        &self,
        Parameters(RenamePreviewParams {
            project_id,
            file_path,
            line,
            character,
            new_name,
            position_encoding,
        }): Parameters<RenamePreviewParams>,
    ) -> Result<Json<StructuredObject>, McpError> {
        let id = parse_project_id(project_id)?;
        let encoding = parse_position_encoding(position_encoding.as_deref())?;
        self.preview_generated_edit(
            &id,
            GeneratedEditRequest::Rename {
                file_path,
                line,
                character,
                new_name,
            },
            encoding,
        )
        .await
    }

    /// Request document formatting from the project LSP and preview the edit.
    #[tool(
        description = "Preview LSP document formatting as a session-owned workspace edit plan. Apply the returned plan from this MCP session with workspace_edit_apply."
    )]
    async fn format_preview(
        &self,
        Parameters(FormatPreviewParams {
            project_id,
            file_path,
            tab_size,
            insert_spaces,
            position_encoding,
            range,
        }): Parameters<FormatPreviewParams>,
    ) -> Result<Json<StructuredObject>, McpError> {
        let id = parse_project_id(project_id)?;
        let encoding = parse_position_encoding(position_encoding.as_deref())?;
        match range {
            Some(range) => {
                self.preview_generated_supported_edit(
                    &id,
                    GeneratedEditRequest::RangeFormat {
                        file_path,
                        start: (range.start_line, range.start_character),
                        end: (range.end_line, range.end_character),
                        tab_size,
                        insert_spaces,
                    },
                    encoding,
                )
                .await
            }
            None => {
                self.preview_generated_edit(
                    &id,
                    GeneratedEditRequest::Format {
                        file_path,
                        tab_size,
                        insert_spaces,
                    },
                    encoding,
                )
                .await
            }
        }
    }

    /// Preview standard range formatting when the project server supports it.
    #[tool(
        description = "Preview capability-gated LSP range formatting as a session-owned edit plan. Unsupported servers return supported=false without creating a plan. Apply changed previews with workspace_edit_apply."
    )]
    async fn range_format_preview(
        &self,
        Parameters(params): Parameters<RangeFormatPreviewParams>,
    ) -> Result<Json<StructuredObject>, McpError> {
        self.format_preview(Parameters(FormatPreviewParams {
            project_id: params.project_id,
            file_path: params.file_path,
            tab_size: params.tab_size,
            insert_spaces: params.insert_spaces,
            position_encoding: params.position_encoding,
            range: Some(FormatRange {
                start_line: params.start_line,
                start_character: params.start_character,
                end_line: params.end_line,
                end_character: params.end_character,
            }),
        }))
        .await
    }

    /// Preview rust-analyzer's syntax-aware item movement extension.
    #[tool(
        description = "Preview capability-gated rust-analyzer item movement up or down as a session-owned edit plan. No-op and unsupported responses create no plan. Snippet edits containing unresolved placeholders fail closed. Apply changed previews with workspace_edit_apply."
    )]
    async fn move_item_preview(
        &self,
        Parameters(params): Parameters<MoveItemPreviewParams>,
    ) -> Result<Json<StructuredObject>, McpError> {
        let id = parse_project_id(params.project_id)?;
        let encoding = parse_position_encoding(params.position_encoding.as_deref())?;
        self.preview_generated_supported_edit(
            &id,
            GeneratedEditRequest::MoveItem {
                file_path: params.file_path,
                start: (params.start_line, params.start_character),
                end: (params.end_line, params.end_character),
                direction: params.direction,
            },
            encoding,
        )
        .await
    }

    /// Preview moving one inline Rust module to its own file.
    #[tool(
        description = "Preview moving a top-level inline Rust module to its own file using the project's current document state. Prefers rust-analyzer's native move_module_to_file edit and falls back to MCPLS's structural ast-grep refactor when unavailable; the response reports the selected producer. Supports raw and Unicode Rust identifiers. File-relative include/path constructs and ambiguous or nested modules fail closed. Apply the returned plan with workspace_edit_apply."
    )]
    async fn move_inline_module_preview(
        &self,
        Parameters(MoveInlineModulePreviewParams {
            project_id,
            file_path,
            module_name,
            module_line,
            module_character,
            position_encoding,
        }): Parameters<MoveInlineModulePreviewParams>,
    ) -> Result<Json<StructuredObject>, McpError> {
        let id = parse_project_id(project_id)?;
        let encoding = parse_position_encoding(position_encoding.as_deref())?;
        let module_position = match (module_line, module_character) {
            (Some(line), Some(character)) => Some(lsp_types::Position { line, character }),
            (None, None) => None,
            _ => {
                return Err(McpError::invalid_params(
                    "module_line and module_character must be provided together".to_string(),
                    None,
                ));
            }
        };
        let artifact = self
            .context
            .project_registry
            .preview_inline_module_move(&id, file_path, module_name, module_position, encoding)
            .await
            .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
        self.context.remember_plan(artifact.plan.id().clone()).await;
        encode_json(&preview_artifact_json(&artifact, id.as_str()))
    }

    /// Preview a filesystem path rename composed with language-server reference updates.
    #[tool(
        description = "Preview one project-contained file or directory rename. MCPLS validates both paths first, asks matching workspace/willRenameFiles providers for reference/module edits, appends exactly one RenameFile operation, and stores the atomic result as a session-owned plan. If no provider supplies semantic edits, verification is structural_unverified rather than semantic_verified. Apply with workspace_edit_apply."
    )]
    async fn path_rename_preview(
        &self,
        Parameters(PathRenamePreviewParams {
            project_id,
            old_path,
            new_path,
            position_encoding,
        }): Parameters<PathRenamePreviewParams>,
    ) -> Result<Json<StructuredObject>, McpError> {
        let id = parse_project_id(project_id)?;
        let encoding = parse_position_encoding(position_encoding.as_deref())?;
        let result = self
            .context
            .project_registry
            .path_rename_preview(
                &id,
                PathRenameRequest {
                    old_path,
                    new_path,
                    encoding,
                },
            )
            .await
            .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
        self.context
            .remember_plan(result.artifact.plan.id().clone())
            .await;
        encode_json(&path_rename_preview_json(&result, id.as_str()))
    }

    /// Search or preview a replacement using one explicitly selected structural dialect.
    #[tool(
        description = "Validate, search, or preview structural replacements without changing files. dialect is required and must be rust_analyzer_ssr (the complete rust-analyzer rule belongs in query) or ast_grep (language_id required; replacement optional for search-only). MCPLS never translates syntax or silently switches engines. A matching replacement returns a session-owned plan for workspace_edit_apply."
    )]
    async fn structural_replace_preview(
        &self,
        Parameters(StructuralReplacePreviewParams {
            project_id,
            file_path,
            dialect,
            query,
            replacement,
            language_id,
            parse_only,
            position_encoding,
        }): Parameters<StructuralReplacePreviewParams>,
    ) -> Result<Json<StructuredObject>, McpError> {
        let id = parse_project_id(project_id)?;
        let dialect = parse_structural_dialect(&dialect)?;
        let encoding = parse_position_encoding(position_encoding.as_deref())?;
        let result = self
            .context
            .project_registry
            .structural_replace_preview(
                &id,
                StructuralReplaceRequest {
                    file_path,
                    dialect,
                    query,
                    replacement,
                    language_id,
                    parse_only,
                    encoding,
                },
            )
            .await
            .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
        if let Some(artifact) = &result.artifact {
            self.context.remember_plan(artifact.plan.id().clone()).await;
        }
        encode_json(&bounded_structural_preview_json(
            &self.context.project_registry,
            &id,
            &result,
        )?)
    }

    /// Apply a previously previewed, session-owned workspace edit plan.
    #[tool(
        name = "workspace_edit_apply",
        output_schema = rmcp::handler::server::tool::schema_for_output::<WorkspaceEditApplyResult>(),
        description = "Apply one workspace edit plan previewed by this MCP session, by project ID and opaque plan ID. The first call revalidates before replacing files; retries after commit return the original receipt without applying again.",
        annotations(
            title = "Apply workspace edit",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true
        )
    )]
    async fn workspace_edit_apply_tool(
        &self,
        Parameters(params): Parameters<WorkspaceEditApplyParams>,
        RequestState(request_state): RequestState,
        ToolInputResponses(input_responses): ToolInputResponses,
        cancellation: CancellationToken,
    ) -> Result<CallToolResponse, McpError> {
        self.apply_project_plan_with_approval(
            "workspace_edit_apply",
            params,
            request_state,
            input_responses,
            cancellation,
        )
        .await
    }

    #[cfg(test)]
    async fn workspace_edit_apply(
        &self,
        Parameters(params): Parameters<WorkspaceEditApplyParams>,
    ) -> Result<Json<WorkspaceEditApplyResult>, McpError> {
        self.apply_project_plan_params(params).await
    }

    /// Apply a rename preview through the generic workspace-edit transaction.
    #[tool(
        name = "rename_apply",
        output_schema = rmcp::handler::server::tool::schema_for_output::<WorkspaceEditApplyResult>(),
        description = "Apply a rename plan returned by rename_preview. The first call revalidates before replacing files; retries after commit return the original receipt without applying again.",
        annotations(
            title = "Apply rename",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true
        )
    )]
    async fn rename_apply_tool(
        &self,
        Parameters(params): Parameters<WorkspaceEditApplyParams>,
        RequestState(request_state): RequestState,
        ToolInputResponses(input_responses): ToolInputResponses,
        cancellation: CancellationToken,
    ) -> Result<CallToolResponse, McpError> {
        self.apply_project_plan_with_approval(
            "rename_apply",
            params,
            request_state,
            input_responses,
            cancellation,
        )
        .await
    }

    #[cfg(test)]
    async fn rename_apply(
        &self,
        Parameters(params): Parameters<WorkspaceEditApplyParams>,
    ) -> Result<Json<WorkspaceEditApplyResult>, McpError> {
        self.apply_project_plan_params(params).await
    }

    /// Apply a formatting preview through the generic workspace-edit transaction.
    #[tool(
        name = "format_apply",
        output_schema = rmcp::handler::server::tool::schema_for_output::<WorkspaceEditApplyResult>(),
        description = "Apply a formatting plan returned by format_preview. The first call revalidates before replacing files; retries after commit return the original receipt without applying again.",
        annotations(
            title = "Apply formatting",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true
        )
    )]
    async fn format_apply_tool(
        &self,
        Parameters(params): Parameters<WorkspaceEditApplyParams>,
        RequestState(request_state): RequestState,
        ToolInputResponses(input_responses): ToolInputResponses,
        cancellation: CancellationToken,
    ) -> Result<CallToolResponse, McpError> {
        self.apply_project_plan_with_approval(
            "format_apply",
            params,
            request_state,
            input_responses,
            cancellation,
        )
        .await
    }

    #[cfg(test)]
    async fn format_apply(
        &self,
        Parameters(params): Parameters<WorkspaceEditApplyParams>,
    ) -> Result<Json<WorkspaceEditApplyResult>, McpError> {
        self.apply_project_plan_params(params).await
    }

    /// Restart the language-server actor for one project.
    #[tool(description = "Restart the language servers for a registered project.")]
    async fn project_restart_lsp(
        &self,
        Parameters(ProjectIdParams { project_id }): Parameters<ProjectIdParams>,
    ) -> Result<Json<StructuredObject>, McpError> {
        let id = parse_project_id(project_id)?;
        let identity = self
            .context
            .project_registry
            .identity(&id)
            .await
            .map_err(|error| McpError::internal_error(error.to_string(), None))?;
        let state = self
            .context
            .project_registry
            .restart(&id)
            .await
            .map_err(|error| McpError::internal_error(error.to_string(), None))?;
        self.project_state_json(&id, &identity, &state).await
    }

    /// Replace one project's Cargo feature profile and restart its actors.
    #[tool(
        description = "Replace the Cargo feature profile for a registered Rust project. Existing project settings are preserved; language-server actors are replaced atomically and the new profile is persisted."
    )]
    async fn project_configure_cargo_features(
        &self,
        Parameters(ProjectCargoFeaturesParams {
            project_id,
            features,
            all_features,
            no_default_features,
        }): Parameters<ProjectCargoFeaturesParams>,
    ) -> Result<Json<StructuredObject>, McpError> {
        let id = parse_project_id(project_id)?;
        let state = self
            .context
            .project_registry
            .update_cargo_features(
                &id,
                crate::config::CargoFeatureProfile {
                    features,
                    all_features,
                    no_default_features,
                },
            )
            .await
            .map_err(|error| McpError::internal_error(error.to_string(), None))?;
        let identity = self
            .context
            .project_registry
            .identity(&id)
            .await
            .map_err(|error| McpError::internal_error(error.to_string(), None))?;
        self.project_state_json(&id, &identity, &state).await
    }

    /// Refresh one project actor's observable state.
    #[tool(description = "Refresh the status of a registered project.")]
    async fn project_refresh(
        &self,
        Parameters(ProjectIdParams { project_id }): Parameters<ProjectIdParams>,
    ) -> Result<Json<StructuredObject>, McpError> {
        let id = parse_project_id(project_id)?;
        let identity = self
            .context
            .project_registry
            .identity(&id)
            .await
            .map_err(|error| McpError::internal_error(error.to_string(), None))?;
        let state = self
            .context
            .project_registry
            .refresh(&id)
            .await
            .map_err(|error| McpError::internal_error(error.to_string(), None))?;
        self.project_state_json(&id, &identity, &state).await
    }

    /// Get hover information at a position in a file.
    #[tool(
        description = "Type, signature/documentation, provider identity, snapshot-bound symbol_handle, and bounded source frame. Pass project_id + symbol_handle from discovery instead of rereading or copying coordinates."
    )]
    async fn get_hover(
        &self,
        Parameters(HoverParams {
            file_path,
            line,
            character,
            project_id,
            symbol_handle,
        }): Parameters<HoverParams>,
    ) -> Result<Json<crate::bridge::HoverResult>, McpError> {
        let (actor, file_path, line, character) = self
            .semantic_target(project_id, symbol_handle, file_path, line, character)
            .await?;
        let result = actor
            .hover(file_path, line, character)
            .await
            .map_err(|error| error.to_string());

        encode_tool_result(result)
    }

    /// Get the definition location of a symbol.
    #[tool(
        description = "Bounded definition targets with provider identity, source frames, reusable symbol_handle values, and explicit truncation. Returned source usually removes the need for a file read."
    )]
    async fn get_definition(
        &self,
        Parameters(DefinitionParams {
            file_path,
            line,
            character,
            project_id,
            symbol_handle,
        }): Parameters<DefinitionParams>,
    ) -> Result<Json<crate::bridge::DefinitionResult>, McpError> {
        let (actor, file_path, line, character) = self
            .semantic_target(project_id, symbol_handle, file_path, line, character)
            .await?;
        let result = actor
            .definition(file_path, line, character)
            .await
            .map_err(|error| error.to_string());

        encode_tool_result(result)
    }

    /// Get the declaration location, distinct from a symbol's definition.
    #[tool(
        description = "Project-scoped declaration targets with bounded source frames and symbol handles. Returns supported=false when unavailable."
    )]
    async fn get_declaration(
        &self,
        Parameters(params): Parameters<SemanticPositionParams>,
    ) -> Result<Json<crate::bridge::SemanticDiscoveryResult>, McpError> {
        self.semantic_discovery(params, SemanticDiscoveryKind::Declaration)
            .await
    }

    /// Locate the Rust module containing a position.
    #[tool(
        description = "Project-scoped rust-analyzer parent-module target with a bounded source frame and symbol handle. Returns supported=false when unavailable."
    )]
    async fn get_parent_module(
        &self,
        Parameters(params): Parameters<SemanticPositionParams>,
    ) -> Result<Json<crate::bridge::SemanticDiscoveryResult>, McpError> {
        self.semantic_discovery(params, SemanticDiscoveryKind::ParentModule)
            .await
    }

    /// Locate child Rust modules declared at a position.
    #[tool(
        description = "Project-scoped rust-analyzer child-module targets with bounded source frames and symbol handles. Returns supported=false when unavailable."
    )]
    async fn get_child_modules(
        &self,
        Parameters(params): Parameters<SemanticPositionParams>,
    ) -> Result<Json<crate::bridge::SemanticDiscoveryResult>, McpError> {
        self.semantic_discovery(params, SemanticDiscoveryKind::ChildModules)
            .await
    }

    /// Expand the Rust macro invocation at a position.
    #[tool(
        description = "Project-scoped rust-analyzer macro expansion, bounded to 1 MiB. Returns expansion source as data and never executes anything."
    )]
    async fn expand_macro(
        &self,
        Parameters(params): Parameters<SemanticPositionParams>,
    ) -> Result<Json<crate::bridge::SemanticDiscoveryResult>, McpError> {
        self.semantic_discovery(params, SemanticDiscoveryKind::MacroExpansion)
            .await
    }

    /// Return nested syntax selections around a position.
    #[tool(
        description = "Project-scoped standard LSP selection-range expansion, ordered from innermost to outermost and bounded to 100 ranges."
    )]
    async fn get_selection_ranges(
        &self,
        Parameters(params): Parameters<SemanticPositionParams>,
    ) -> Result<Json<crate::bridge::SemanticDiscoveryResult>, McpError> {
        self.semantic_discovery(params, SemanticDiscoveryKind::SelectionRanges)
            .await
    }

    /// Discover rust-analyzer runnables without executing them.
    #[tool(
        description = "Project-scoped rust-analyzer runnable discovery. Returns bounded serialized command, args, environment, cwd, and location data; MCPLS never executes it."
    )]
    async fn discover_runnables(
        &self,
        Parameters(params): Parameters<SemanticPositionParams>,
    ) -> Result<Json<crate::bridge::SemanticDiscoveryResult>, McpError> {
        self.semantic_discovery(params, SemanticDiscoveryKind::Runnables)
            .await
    }

    /// Discover tests related to the symbol at a position without executing them.
    #[tool(
        description = "Project-scoped rust-analyzer related-test discovery as bounded runnable data. MCPLS never executes returned commands."
    )]
    async fn discover_related_tests(
        &self,
        Parameters(params): Parameters<SemanticPositionParams>,
    ) -> Result<Json<crate::bridge::SemanticDiscoveryResult>, McpError> {
        self.semantic_discovery(params, SemanticDiscoveryKind::RelatedTests)
            .await
    }

    /// Find all references to a symbol.
    #[tool(
        description = "References grouped by project-relative file and enclosing symbol, with one declaration, bounded source frames, reusable handles, counts, and truncation metadata."
    )]
    async fn get_references(
        &self,
        Parameters(ReferencesParams {
            file_path,
            line,
            character,
            project_id,
            symbol_handle,
            include_declaration,
            limits,
            page_token,
        }): Parameters<ReferencesParams>,
    ) -> Result<Json<crate::bridge::ReferencesResult>, McpError> {
        let (actor, file_path, line, character) = self
            .semantic_target(project_id, symbol_handle, file_path, line, character)
            .await?;
        let page_offset = page_token
            .as_deref()
            .map(|token| {
                token.parse::<usize>().map_err(|_| {
                    McpError::invalid_params(
                        "page_token must be the decimal next_cursor returned by get_references",
                        None,
                    )
                })
            })
            .transpose()?;
        let page_offset = page_offset.or(Some(0));
        let result = actor
            .references_with_cursor(
                file_path,
                line,
                character,
                include_declaration,
                limits,
                page_offset,
            )
            .await
            .map_err(|error| error.to_string());

        encode_tool_result(result)
    }

    /// Get diagnostics for a file.
    #[tool(
        description = "Get diagnostics with mode=cached_preferred (default), fresh, or cache_only. cache_only never starts provider analysis."
    )]
    async fn get_diagnostics(
        &self,
        Parameters(DiagnosticsParams {
            file_path,
            mode,
            fresh,
            options,
        }): Parameters<DiagnosticsParams>,
    ) -> Result<Json<crate::bridge::DiagnosticsResult>, McpError> {
        let actor = self
            .context
            .required_actor_for_path(&file_path)
            .await
            .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
        let mode = match fresh {
            Some(true) if mode == DiagnosticsMode::CachedPreferred => DiagnosticsMode::Fresh,
            Some(false) if mode == DiagnosticsMode::CachedPreferred => DiagnosticsMode::CacheOnly,
            Some(_) if mode != DiagnosticsMode::CachedPreferred => {
                return Err(McpError::invalid_params(
                    "fresh cannot be combined with mode".to_owned(),
                    None,
                ));
            }
            _ => mode,
        };
        let result = if mode == DiagnosticsMode::Fresh {
            actor
                .diagnostics_with_options(file_path, options)
                .await
                .map_err(|error| error.to_string())
        } else {
            actor
                .cached_diagnostics_with_options(file_path, options)
                .await
                .map_err(|error| error.to_string())
        };

        encode_tool_result(result)
    }

    /// Rename a symbol across the workspace.
    #[tool(
        description = "Rename symbol across workspace. Returns text edits for all files where symbol is used."
    )]
    async fn rename_symbol(
        &self,
        Parameters(RenameParams {
            file_path,
            line,
            character,
            new_name,
        }): Parameters<RenameParams>,
    ) -> Result<Json<crate::bridge::RenameResult>, McpError> {
        let actor = self
            .context
            .required_actor_for_path(&file_path)
            .await
            .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
        let result = actor
            .rename(file_path, line, character, new_name)
            .await
            .map_err(|error| error.to_string());

        encode_tool_result(result)
    }

    /// Get code completion suggestions.
    #[tool(
        description = "Completion suggestions at position. Returns methods, functions, variables, types, and snippets."
    )]
    async fn get_completions(
        &self,
        Parameters(CompletionsParams {
            file_path,
            line,
            character,
            trigger,
        }): Parameters<CompletionsParams>,
    ) -> Result<Json<crate::bridge::CompletionsResult>, McpError> {
        let actor = self
            .context
            .required_actor_for_path(&file_path)
            .await
            .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
        let result = actor
            .completions(file_path, line, character, trigger)
            .await
            .map_err(|error| error.to_string());

        encode_tool_result(result)
    }

    /// Get all symbols in a document.
    #[tool(
        description = "Bounded, pageable semantic outline for one file. Returns flat declaration entries with exact parent handles, a shared snapshot source reference, and reusable symbol handles."
    )]
    async fn get_document_symbols(
        &self,
        Parameters(DocumentSymbolsParams {
            file_path,
            options,
            max_bytes,
            page_token,
        }): Parameters<DocumentSymbolsParams>,
    ) -> Result<Json<crate::bridge::DocumentSymbolsResult>, McpError> {
        let actor = self
            .context
            .required_actor_for_path(&file_path)
            .await
            .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
        let result = actor
            .document_symbol_page(crate::bridge::DocumentSymbolPageRequest {
                file_path,
                options,
                max_bytes,
                page_token,
            })
            .await
            .map_err(|error| error.to_string());

        encode_tool_result(result)
    }

    /// Format a document according to language server rules.
    #[tool(
        description = "Format document with language-specific rules. Returns text edits for indentation, spacing, and style."
    )]
    async fn format_document(
        &self,
        Parameters(FormatDocumentParams {
            file_path,
            tab_size,
            insert_spaces,
        }): Parameters<FormatDocumentParams>,
    ) -> Result<Json<crate::bridge::FormatDocumentResult>, McpError> {
        let actor = self
            .context
            .required_actor_for_path(&file_path)
            .await
            .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
        let result = actor
            .format_document(file_path, tab_size, insert_spaces)
            .await
            .map_err(|error| error.to_string());

        encode_tool_result(result)
    }

    /// Search one symbol query or several queries across the workspace.
    #[tool(
        output_schema = rmcp::handler::server::tool::schema_for_output::<WorkspaceSymbolSearchResponse>(),
        description = "Search one query (query) or several caller-ordered queries (queries). Batch duplicates reuse provider work; results have bounded source frames and reusable symbol handles."
    )]
    async fn workspace_symbol_search(
        &self,
        Parameters(params): Parameters<WorkspaceSymbolParams>,
    ) -> Result<Json<WorkspaceSymbolSearchResponse>, McpError> {
        if !params.queries.is_empty() {
            if params.query.is_some() || params.page_token.is_some() {
                return Err(McpError::invalid_params(
                    "queries cannot be combined with query or page_token",
                    None,
                ));
            }
            let batch = WorkspaceSymbolBatchParams {
                project_id: params.project_id,
                queries: params.queries,
                kind_filter: params.kind_filter,
                match_mode: params.match_mode,
                scope: params.scope,
                max_items: params.limit,
                max_bytes: params.max_bytes,
                include_generated: params.include_generated,
            };
            return encode_tool_result(
                self.workspace_symbol_batch_result(batch)
                    .await
                    .map(|result| WorkspaceSymbolSearchResponse::Many(Box::new(result))),
            );
        }
        let query = params
            .query
            .ok_or_else(|| McpError::invalid_params("query or queries is required", None))?;
        if params.limit == 0 || params.limit > 1_000 {
            return Err(McpError::invalid_params(
                "limit must be between 1 and 1000",
                None,
            ));
        }
        if !(4_096..=1_048_576).contains(&params.max_bytes) {
            return Err(McpError::invalid_params(
                "max_bytes must be between 4096 and 1048576",
                None,
            ));
        }
        let id = parse_project_id(params.project_id)?;
        let actor = self
            .context
            .project_registry
            .actor_for_project(&id)
            .await
            .map_err(project_routing_error)?;
        let result = actor
            .workspace_symbol(crate::bridge::WorkspaceSymbolPageRequest {
                query,
                kind_filter: params.kind_filter,
                match_mode: params.match_mode,
                scope: params.scope,
                include_generated: params.include_generated,
                max_items: params.limit as usize,
                max_bytes: params.max_bytes,
                page_token: params.page_token,
            })
            .await
            .map_err(|error| error.to_string())
            .map(|result| WorkspaceSymbolSearchResponse::One(Box::new(result)));

        encode_tool_result(result)
    }

    async fn workspace_symbol_batch_result(
        &self,
        params: WorkspaceSymbolBatchParams,
    ) -> Result<crate::bridge::WorkspaceSymbolBatchResult, String> {
        validate_workspace_symbol_batch(&params).map_err(|error| error.to_string())?;
        let id = parse_project_id(params.project_id).map_err(|error| error.to_string())?;
        let actor = self
            .context
            .project_registry
            .actor_for_project(&id)
            .await
            .map_err(project_routing_error)
            .map_err(|error| error.to_string())?;
        actor
            .workspace_symbol_batch(WorkspaceSymbolBatchRequest {
                queries: params.queries,
                kind_filter: params.kind_filter,
                match_mode: params.match_mode,
                scope: params.scope,
                include_generated: params.include_generated,
                max_items: params.max_items as usize,
                max_bytes: params.max_bytes,
            })
            .await
            .map_err(|error| error.to_string())
    }

    /// Search project snapshots by literal text or Rust regex.
    #[tool(
        output_schema = rmcp::handler::server::tool::schema_for_output::<LexicalSearchResponse>(),
        description = "Bounded project lexical search over current document snapshots. Use query for one search or queries for caller-ordered searches sharing one source scan and response budget."
    )]
    async fn lexical_search(
        &self,
        Parameters(params): Parameters<LexicalSearchParams>,
    ) -> Result<Json<serde_json::Value>, McpError> {
        if params.max_files == 0 || params.max_matches == 0 {
            return Err(McpError::invalid_params(
                "max_files and max_matches must be greater than zero",
                None,
            ));
        }
        if !(4 * 1024..=1024 * 1024).contains(&params.max_bytes) {
            return Err(McpError::invalid_params(
                "max_bytes must be between 4096 and 1048576",
                None,
            ));
        }
        let mut queries = params.queries;
        if let Some(query) = params.query {
            if !queries.is_empty() {
                return Err(McpError::invalid_params(
                    "query cannot be combined with queries",
                    None,
                ));
            }
            queries.push(query);
        }
        if queries.is_empty() {
            return Err(McpError::invalid_params(
                "query or queries is required",
                None,
            ));
        }
        if params.page_token.is_some() && queries.len() != 1 {
            return Err(McpError::invalid_params(
                "page_token requires one query",
                None,
            ));
        }
        for query in &queries {
            find_matches("", query, params.mode, params.case, params.multiline)
                .map_err(|error| McpError::invalid_params(error, None))?;
        }
        validate_path_globs(&params.include_paths, &params.exclude_paths)
            .map_err(|error| McpError::invalid_params(error, None))?;
        let limit = params.max_matches;
        let id = parse_project_id(params.project_id)?;
        let actor = self
            .context
            .project_registry
            .actor_for_project(&id)
            .await
            .map_err(project_routing_error)?;
        let max_bytes = effective_lexical_page_bytes(params.max_bytes);
        let value = if queries.len() == 1 {
            let scan = actor
                .lexical_search(LexicalSearchRequest {
                    query: queries.remove(0),
                    mode: params.mode,
                    case: params.case,
                    multiline: params.multiline,
                    max_files: params.max_files,
                    max_matches: limit,
                    include_generated: params.include_generated,
                    include_paths: params.include_paths,
                    exclude_paths: params.exclude_paths,
                    context_lines: params.context_lines,
                    page_token: params.page_token,
                })
                .await
                .map_err(|error| McpError::internal_error(error.to_string(), None))?;
            let has_next_page =
                scan.offset.saturating_add(scan.matches.len()) < scan.total_matches;
            let page = bounded_lexical_page_with_accounting(
                scan.matches,
                scan.offset,
                has_next_page,
                max_bytes,
                scan.total_matches,
                scan.scanned_files,
                scan.scanned_bytes,
                Some(&scan.page_token),
                &scan.snapshot_identity,
            )
            .map_err(|required_bytes| {
                McpError::invalid_params(
                    format!(
                        "max_bytes must be at least {required_bytes} to return one lexical match identity"
                    ),
                    None,
                )
            })?;
            serde_json::to_value(LexicalSearchResponse::One(Box::new(page)))
        } else {
            let batch = actor
                .lexical_search_batch(LexicalSearchBatchRequest {
                    queries,
                    mode: params.mode,
                    case: params.case,
                    multiline: params.multiline,
                    max_files: params.max_files,
                    max_matches: limit,
                    include_generated: params.include_generated,
                    include_paths: params.include_paths,
                    exclude_paths: params.exclude_paths,
                    context_lines: params.context_lines,
                    max_bytes,
                })
                .await
                .map_err(|error| McpError::internal_error(error.to_string(), None))?;
            serde_json::to_value(LexicalSearchResponse::Many(Box::new(
                bounded_lexical_batch(batch, max_bytes).map_err(|required_bytes| {
                    McpError::invalid_params(
                        format!("max_bytes must be at least {required_bytes} for lexical batch metadata"),
                        None,
                    )
                })?,
            )))
        }
            .map_err(|error| McpError::internal_error(error.to_string(), None))?;
        let legacy = value.to_string();
        Ok(Json::new(value, legacy))
    }

    /// Search several symbol names through one bounded actor request.
    #[tool(
        description = "Batch workspace-symbol search with shared filters and global item/byte bounds. Exact duplicate queries reuse the first entry instead of repeating provider work or payloads."
    )]
    async fn workspace_symbol_search_batch(
        &self,
        Parameters(params): Parameters<WorkspaceSymbolBatchParams>,
    ) -> Result<Json<crate::bridge::WorkspaceSymbolBatchResult>, McpError> {
        encode_tool_result(self.workspace_symbol_batch_result(params).await)
    }

    /// Resolve and inspect one symbol or several symbols without rereading files.
    #[tool(
        output_schema = rmcp::handler::server::tool::schema_for_output::<InspectSymbolResponse>(),
        description = "Inspect one query or symbol handle, or 1-16 caller-ordered targets, with bounded source frames. Multi-target results use retained 16 KiB pages; continuation reuses the original provider work."
    )]
    async fn inspect_symbol(
        &self,
        Parameters(params): Parameters<InspectSymbolParams>,
    ) -> Result<Json<InspectSymbolResponse>, McpError> {
        if !params.targets.is_empty() || params.page_token.is_some() {
            if params.symbol_handle.is_some()
                || params.query.is_some()
                || params.kind.is_some()
                || params.path.is_some()
                || params.container.is_some()
            {
                return Err(McpError::invalid_params(
                    "targets or page_token cannot be combined with a single-symbol identity",
                    None,
                ));
            }
            return encode_tool_result(
                self.inspect_symbol_batch_result(InspectSymbolBatchParams {
                    project_id: params.project_id,
                    targets: params.targets,
                    candidate_limit: params.candidate_limit,
                    sections: params.sections,
                    budget: params.budget,
                    page_token: params.page_token,
                })
                .await
                .map(|result| InspectSymbolResponse::Many(Box::new(result))),
            );
        }
        if params.symbol_handle.is_none() && params.query.as_ref().is_none_or(String::is_empty) {
            return Err(McpError::invalid_params(
                "query or symbol_handle is required",
                None,
            ));
        }
        if params.budget.max_bytes < 4_096 || params.budget.max_items == 0 {
            return Err(McpError::invalid_params(
                "budget.max_bytes must be at least 4096 and budget.max_items must be positive",
                None,
            ));
        }
        let id = parse_project_id(params.project_id)?;
        let actor = self
            .context
            .project_registry
            .actor_for_project(&id)
            .await
            .map_err(project_operation_error)?;
        let result = actor
            .inspect_symbol(crate::bridge::InspectSymbolRequest {
                symbol_handle: params.symbol_handle,
                query: params.query,
                kind: params.kind,
                path: params.path,
                container: params.container,
                candidate_limit: params.candidate_limit,
                sections: params.sections,
                budget: params.budget,
            })
            .await
            .map_err(operation_error)
            .map(|result| InspectSymbolResponse::One(Box::new(result)));
        encode_tool_result(result)
    }

    async fn inspect_symbol_batch_result(
        &self,
        params: InspectSymbolBatchParams,
    ) -> Result<crate::bridge::InspectSymbolBatchResult, String> {
        validate_inspect_symbol_batch(&params).map_err(|error| error.to_string())?;
        let id = parse_project_id(params.project_id).map_err(|error| error.to_string())?;
        let actor = self
            .context
            .project_registry
            .actor_for_project(&id)
            .await
            .map_err(project_operation_error)
            .map_err(|error| error.to_string())?;
        actor
            .inspect_symbol_batch(crate::bridge::InspectSymbolBatchRequest {
                targets: params.targets,
                candidate_limit: params.candidate_limit,
                sections: params.sections,
                budget: params.budget,
                page_token: params.page_token,
            })
            .await
            .map_err(operation_error)
            .map_err(|error| error.to_string())
    }

    /// Inspect several symbols concurrently without repeating actor round trips.
    #[tool(
        description = "Inspect 1-16 symbol handles or queries concurrently, returning caller-ordered source-bearing 16 KiB pages. Continue with page_token and no targets; provider work is retained rather than repeated."
    )]
    async fn inspect_symbol_batch(
        &self,
        Parameters(params): Parameters<InspectSymbolBatchParams>,
    ) -> Result<Json<crate::bridge::InspectSymbolBatchResult>, McpError> {
        encode_tool_result(self.inspect_symbol_batch_result(params).await)
    }

    /// Get code actions for a range.
    #[tool(
        description = "Code actions for range. Returns quick fixes, refactorings, and source actions with edits."
    )]
    async fn get_code_actions(
        &self,
        Parameters(CodeActionsParams {
            file_path,
            start_line,
            start_character,
            end_line,
            end_character,
            kind_filter,
        }): Parameters<CodeActionsParams>,
    ) -> Result<Json<crate::bridge::CodeActionsResult>, McpError> {
        let actor = self
            .context
            .required_actor_for_path(&file_path)
            .await
            .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
        let result = actor
            .code_actions(
                file_path,
                start_line,
                start_character,
                end_line,
                end_character,
                kind_filter,
            )
            .await
            .map_err(|error| error.to_string());

        encode_tool_result(result)
    }

    /// List project-scoped code actions with bounded reusable references.
    #[tool(description = "List code actions and return project-scoped references for preview.")]
    async fn code_action_list(
        &self,
        Parameters(CodeActionListParams {
            project_id,
            file_path,
            start_line,
            start_character,
            end_line,
            end_character,
            kind_filter,
        }): Parameters<CodeActionListParams>,
    ) -> Result<Json<crate::bridge::CodeActionsResult>, McpError> {
        let id = parse_project_id(project_id)?;
        let result = self
            .context
            .project_registry
            .code_action_list(
                &id,
                file_path,
                start_line,
                start_character,
                end_line,
                end_character,
                kind_filter,
            )
            .await;
        encode_tool_result(result)
    }

    /// Resolve and preview one project-scoped code action.
    #[tool(
        description = "Preview a code action using its project-scoped reference; the returned plan is owned by this MCP session."
    )]
    async fn code_action_preview(
        &self,
        Parameters(CodeActionPreviewParams {
            project_id,
            action_id,
            position_encoding,
        }): Parameters<CodeActionPreviewParams>,
    ) -> Result<Json<StructuredObject>, McpError> {
        let id = parse_project_id(project_id)?;
        let action_id = PlanId::parse(action_id)
            .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
        let encoding = parse_position_encoding(position_encoding.as_deref())?;
        let result = self
            .context
            .project_registry
            .preview_code_action(&id, action_id, encoding)
            .await
            .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
        self.context.remember_plan(result.plan.id().clone()).await;
        encode_json(&preview_artifact_json(&result, id.as_str()))
    }

    /// Apply a code action plan previewed by this MCP session.
    #[tool(
        name = "code_action_apply",
        output_schema = rmcp::handler::server::tool::schema_for_output::<WorkspaceEditApplyResult>(),
        description = "Apply a code action preview plan owned by this MCP session. Retries after commit return the original receipt without applying again.",
        annotations(
            title = "Apply code action",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true
        )
    )]
    async fn code_action_apply_tool(
        &self,
        Parameters(CodeActionApplyParams {
            project_id,
            plan_id,
            wait_timeout_ms,
        }): Parameters<CodeActionApplyParams>,
        RequestState(request_state): RequestState,
        ToolInputResponses(input_responses): ToolInputResponses,
        cancellation: CancellationToken,
    ) -> Result<CallToolResponse, McpError> {
        self.apply_project_plan_with_approval(
            "code_action_apply",
            WorkspaceEditApplyParams {
                project_id,
                plan_id,
                wait_timeout_ms,
            },
            request_state,
            input_responses,
            cancellation,
        )
        .await
    }

    /// Prepare call hierarchy at a position.
    #[tool(
        description = "Prepare bounded call-hierarchy targets with provider identity, source frames, and symbol handles for follow-up analysis."
    )]
    async fn prepare_call_hierarchy(
        &self,
        Parameters(CallHierarchyPrepareParams {
            file_path,
            line,
            character,
            project_id,
            page_token,
            symbol_handle,
        }): Parameters<CallHierarchyPrepareParams>,
    ) -> Result<Json<crate::bridge::CallHierarchyPrepareResult>, McpError> {
        let (actor, file_path, line, character) = if page_token.is_some() {
            let actor = if let Some(project_id) = project_id {
                let id = parse_project_id(project_id)?;
                self.context.required_actor_for_project(&id).await
            } else if file_path.is_empty() {
                return Err(McpError::invalid_params(
                    "page_token requires project_id or file_path".to_owned(),
                    None,
                ));
            } else {
                self.context.required_actor_for_path(&file_path).await
            }
            .map_err(project_routing_error)?;
            (actor, file_path, line, character)
        } else {
            self.semantic_target(project_id, symbol_handle, file_path, line, character)
                .await?
        };
        let result = actor
            .prepare_call_hierarchy(file_path, line, character, page_token)
            .await
            .map_err(|error| error.to_string());

        encode_tool_result(result)
    }

    /// Get incoming calls (callers).
    #[tool(
        description = "Functions calling the specified item, with caller declarations and bounded source for every returned call site."
    )]
    async fn get_incoming_calls(
        &self,
        Parameters(params): Parameters<CallHierarchyCallsParams>,
    ) -> Result<Json<crate::bridge::IncomingCallsResult>, McpError> {
        let (actor, item, limits) = self.call_hierarchy_target(params).await?;
        let result = actor
            .incoming_calls(item, limits)
            .await
            .map_err(|error| error.to_string());

        encode_tool_result(result)
    }

    /// Get outgoing calls (callees).
    #[tool(
        description = "Functions called by the specified item, with callee declarations and bounded source for every returned call site."
    )]
    async fn get_outgoing_calls(
        &self,
        Parameters(params): Parameters<CallHierarchyCallsParams>,
    ) -> Result<Json<crate::bridge::OutgoingCallsResult>, McpError> {
        let (actor, item, limits) = self.call_hierarchy_target(params).await?;
        let result = actor
            .outgoing_calls(item, limits)
            .await
            .map_err(|error| error.to_string());

        encode_tool_result(result)
    }

    /// Get cached diagnostics for a file.
    #[tool(
        description = "Cached diagnostics from server notifications. Faster than get_diagnostics, no new analysis."
    )]
    async fn get_cached_diagnostics(
        &self,
        Parameters(CachedDiagnosticsParams { file_path, options }): Parameters<
            CachedDiagnosticsParams,
        >,
    ) -> Result<Json<crate::bridge::DiagnosticsResult>, McpError> {
        self.get_diagnostics(Parameters(DiagnosticsParams {
            file_path,
            mode: DiagnosticsMode::CacheOnly,
            fresh: None,
            options,
        }))
        .await
    }

    /// Get recent LSP server log messages.
    #[tool(
        description = "Recent server log messages. Filter by level (error, warning, info, debug) for debugging."
    )]
    async fn get_server_logs(
        &self,
        Parameters(ServerLogsParams {
            project_id,
            limit,
            min_level,
        }): Parameters<ServerLogsParams>,
    ) -> Result<Json<crate::bridge::ServerLogsResult>, McpError> {
        let id = parse_project_id(project_id)?;
        encode_tool_result(
            self.context
                .project_registry
                .server_logs(&id, limit, min_level)
                .await,
        )
    }

    /// Get recent LSP server messages.
    #[tool(
        description = "Recent server messages (showMessage notifications). User-facing prompts and status updates."
    )]
    async fn get_server_messages(
        &self,
        Parameters(ServerMessagesParams { project_id, limit }): Parameters<ServerMessagesParams>,
    ) -> Result<Json<crate::bridge::ServerMessagesResult>, McpError> {
        let id = parse_project_id(project_id)?;
        encode_tool_result(
            self.context
                .project_registry
                .server_messages(&id, limit)
                .await,
        )
    }

    /// Inspect negotiated capabilities for a registered project's active servers.
    #[tool(
        description = "Negotiated capabilities for a project's active language servers. Optionally filter by language ID."
    )]
    async fn project_lsp_capabilities(
        &self,
        Parameters(ProjectLspCapabilitiesParams {
            project_id,
            language_id,
        }): Parameters<ProjectLspCapabilitiesParams>,
    ) -> Result<Json<ProjectLspCapabilitiesResponse>, McpError> {
        let id = parse_project_id(project_id)?;
        let servers = self
            .context
            .project_registry
            .server_capabilities(&id, language_id)
            .await
            .map_err(|error| McpError::internal_error(error.to_string(), None))?;
        encode_tool_result::<_, std::convert::Infallible>(Ok(ProjectLspCapabilitiesResponse {
            project_id: id.as_str().to_string(),
            servers,
        }))
    }

    /// Get signature help at a position.
    #[tool(
        description = "Signature help at position. Returns parameter info, active signature/parameter, and documentation while typing a call."
    )]
    async fn get_signature_help(
        &self,
        Parameters(SignatureHelpParams {
            file_path,
            line,
            character,
        }): Parameters<SignatureHelpParams>,
    ) -> Result<Json<crate::bridge::SignatureHelpResult>, McpError> {
        let actor = self
            .context
            .required_actor_for_path(&file_path)
            .await
            .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
        let result = actor
            .signature_help(file_path, line, character)
            .await
            .map_err(|error| error.to_string());

        encode_tool_result(result)
    }

    /// Go to implementation locations.
    #[tool(
        description = "Bounded implementation targets with provider identity, source frames, symbol handles, and explicit truncation state."
    )]
    async fn go_to_implementation(
        &self,
        Parameters(GoToImplementationParams {
            file_path,
            line,
            character,
            project_id,
            symbol_handle,
        }): Parameters<GoToImplementationParams>,
    ) -> Result<Json<crate::bridge::LocationsResult>, McpError> {
        let (actor, file_path, line, character) = self
            .semantic_target(project_id, symbol_handle, file_path, line, character)
            .await?;
        let result = actor
            .go_to_implementation(file_path, line, character)
            .await
            .map_err(|error| error.to_string());

        encode_tool_result(result)
    }

    /// Go to type definition location.
    #[tool(
        description = "Bounded type-definition targets with provider identity, source frames, symbol handles, and explicit truncation state."
    )]
    async fn go_to_type_definition(
        &self,
        Parameters(GoToTypeDefinitionParams {
            file_path,
            line,
            character,
            project_id,
            symbol_handle,
        }): Parameters<GoToTypeDefinitionParams>,
    ) -> Result<Json<crate::bridge::LocationsResult>, McpError> {
        let (actor, file_path, line, character) = self
            .semantic_target(project_id, symbol_handle, file_path, line, character)
            .await?;
        let result = actor
            .go_to_type_definition(file_path, line, character)
            .await
            .map_err(|error| error.to_string());

        encode_tool_result(result)
    }

    /// Get inlay hints for a range.
    #[tool(
        description = "Inlay hints in range. Returns inferred type/parameter annotations the editor would render inline."
    )]
    async fn get_inlay_hints(
        &self,
        Parameters(InlayHintsParams {
            file_path,
            start_line,
            start_character,
            end_line,
            end_character,
        }): Parameters<InlayHintsParams>,
    ) -> Result<Json<crate::bridge::InlayHintsResult>, McpError> {
        let actor = self
            .context
            .required_actor_for_path(&file_path)
            .await
            .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
        let result = actor
            .inlay_hints(
                file_path,
                start_line,
                start_character,
                end_line,
                end_character,
            )
            .await
            .map_err(|error| error.to_string());

        encode_tool_result(result)
    }

    /// Read source context omitted from a bounded result when the client does not expose MCP resources.
    #[tool(
        description = "Read a snapshot-bound semantic payload from an mcpls-source:// or mcpls-deferred:// URI returned by another MCPLS tool. Deferred payloads return lossless UTF-8 JSON pages with a continuation URI when needed. This is the callable fallback for clients that do not expose resources/read."
    )]
    async fn read_semantic_resource(
        &self,
        Parameters(SemanticResourceReadParams { uri }): Parameters<SemanticResourceReadParams>,
    ) -> Result<Json<SemanticResourceReadResult>, McpError> {
        let resource = parse_session_resource_uri(&uri)
            .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
        let result = match resource {
            SessionResource::Source(source) => {
                self.read_source_resource_as_tool_result(source).await?
            }
            SessionResource::Deferred(deferred) => {
                let payload = self
                    .context
                    .project_registry
                    .read_deferred_resource(&deferred.token)
                    .map_err(|error| McpError::invalid_params(error, None))?;
                deferred_resource_page(&deferred, uri, payload.value, &payload.snapshot_hash)?
            }
            SessionResource::Diagnostics(_)
            | SessionResource::ProjectStatus(_)
            | SessionResource::ProjectEvents { .. }
            | SessionResource::ProjectEvent { .. }
            | SessionResource::EditDiff { .. }
            | SessionResource::AppliedEditResult { .. }
            | SessionResource::EditApproval { .. } => {
                return Err(McpError::invalid_params(
                    "read_semantic_resource accepts only mcpls-source:// or mcpls-deferred:// references",
                    None,
                ));
            }
        };

        encode_tool_result(Ok::<_, String>(result))
    }
}

impl McplsServer {
    async fn read_source_resource_as_tool_result(
        &self,
        resource: crate::bridge::resources::SourceResource,
    ) -> Result<SemanticResourceReadResult, McpError> {
        let actor = match self.context.required_actor_for_path(&resource.path).await {
            Ok(actor) => actor,
            Err(_) => self
                .context
                .project_registry
                .actor_for_source_path(&resource.path)
                .await
                .map_err(|error| McpError::invalid_params(error.to_string(), None))?,
        };
        let mut frame_budget = MAX_SEMANTIC_RESOURCE_RESULT_BYTES;

        loop {
            let frame = actor
                .read_source_resource(resource.clone(), frame_budget)
                .await
                .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
            let uri = make_source_uri(
                Path::new(&frame.path),
                frame.range.start.line,
                frame.range.start.character,
                frame.range.end.line,
                frame.range.end.character,
                &frame.content_hash,
                frame.document_version,
            )
            .map_err(|error| McpError::internal_error(error.to_string(), None))?;
            let text = serde_json::to_string(&frame)
                .map_err(|error| McpError::internal_error(error.to_string(), None))?;
            let result = SemanticResourceReadResult {
                uri,
                mime_type: "application/json".to_owned(),
                text,
                next_uri: None,
                total_bytes: None,
                offset_bytes: None,
                returned_bytes: None,
                remaining_bytes: None,
                snapshot_hash: None,
            };
            let result_bytes = serde_json::to_vec(&result)
                .map_err(|error| McpError::internal_error(error.to_string(), None))?;
            if result_bytes.len() <= MAX_SEMANTIC_RESOURCE_RESULT_BYTES {
                return Ok(result);
            }

            frame_budget /= 2;
            if frame_budget == 0 {
                return Err(McpError::internal_error(
                    "source resource metadata exceeds the response budget",
                    None,
                ));
            }
        }
    }

    async fn read_source_resource(
        &self,
        resource: crate::bridge::resources::SourceResource,
        _uri: String,
        supports_cache_hints: bool,
    ) -> Result<ReadResourceResponse, McpError> {
        let actor = match self.context.required_actor_for_path(&resource.path).await {
            Ok(actor) => actor,
            Err(_) => self
                .context
                .project_registry
                .actor_for_source_path(&resource.path)
                .await
                .map_err(|error| McpError::invalid_params(error.to_string(), None))?,
        };
        let frame = actor
            .read_source_resource(resource, MAX_SEMANTIC_RESOURCE_RESULT_BYTES)
            .await
            .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
        let fresh_uri = make_source_uri(
            Path::new(&frame.path),
            frame.range.start.line,
            frame.range.start.character,
            frame.range.end.line,
            frame.range.end.character,
            &frame.content_hash,
            frame.document_version,
        )
        .map_err(|error| McpError::internal_error(error.to_string(), None))?;
        let json = serde_json::to_string(&frame)
            .map_err(|error| McpError::internal_error(error.to_string(), None))?;
        Ok(private_resource_result(
            vec![ResourceContents::text(json, fresh_uri)],
            supports_cache_hints,
        )
        .into())
    }

    fn read_deferred_resource(
        &self,
        deferred: DeferredResource,
        uri: String,
        supports_cache_hints: bool,
    ) -> Result<ReadResourceResponse, McpError> {
        let payload = self
            .context
            .project_registry
            .read_deferred_resource(&deferred.token)
            .map_err(|error| McpError::invalid_params(error, None))?;
        let page = deferred_resource_page(
            &deferred,
            uri.clone(),
            payload.value,
            &payload.snapshot_hash,
        )?;
        let json = serde_json::to_string(&page)
            .map_err(|error| McpError::internal_error(error.to_string(), None))?;
        Ok(private_resource_result(
            vec![ResourceContents::text(json, uri)],
            supports_cache_hints,
        )
        .into())
    }

    async fn listen_project_ids(
        &self,
        accepted: &std::collections::HashSet<String>,
    ) -> Result<std::collections::HashSet<ProjectId>, McpError> {
        let mut project_ids = std::collections::HashSet::new();
        for uri in accepted {
            match parse_session_resource_uri(uri)
                .map_err(|error| McpError::invalid_params(error.to_string(), None))?
            {
                SessionResource::ProjectStatus(project_id)
                | SessionResource::ProjectEvents { project_id, .. }
                | SessionResource::ProjectEvent { project_id, .. } => {
                    project_ids.insert(project_id);
                }
                SessionResource::Diagnostics(path) => {
                    let (project_id, _) = self
                        .context
                        .required_project_for_path(path)
                        .await
                        .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
                    project_ids.insert(project_id);
                }
                SessionResource::Source(source) => {
                    let (project_id, _) = self
                        .context
                        .required_project_for_path(source.path)
                        .await
                        .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
                    project_ids.insert(project_id);
                }
                SessionResource::Deferred(_) => {
                    return Err(McpError::invalid_params(
                        "deferred semantic resources are readable but not subscribable",
                        None,
                    ));
                }
                SessionResource::EditDiff { .. } => {
                    return Err(McpError::invalid_params(
                        "edit diff resources are readable but not subscribable",
                        None,
                    ));
                }
                SessionResource::AppliedEditResult { .. } => {
                    return Err(McpError::invalid_params(
                        "applied edit result resources are readable but not subscribable",
                        None,
                    ));
                }
                SessionResource::EditApproval { .. } => {
                    return Err(McpError::invalid_params(
                        "edit approval resources are readable but not subscribable",
                        None,
                    ));
                }
            }
        }
        Ok(project_ids)
    }

    async fn spawn_listen_events(
        &self,
        project_ids: std::collections::HashSet<ProjectId>,
        event_tx: &tokio::sync::mpsc::Sender<ListenEvent>,
    ) -> Result<Vec<tokio::task::JoinHandle<()>>, McpError> {
        let mut tasks = Vec::new();
        for project_id in project_ids {
            let actors = self
                .context
                .project_registry
                .actors_for_project(&project_id)
                .await
                .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
            for actor in actors {
                let mut events = actor.subscribe_events();
                let event_tx = event_tx.clone();
                let project_id = project_id.clone();
                tasks.push(tokio::spawn(async move {
                    loop {
                        match events.recv().await {
                            Ok(event) => {
                                if event_tx
                                    .send(ListenEvent::Event(project_id.clone(), event))
                                    .await
                                    .is_err()
                                {
                                    break;
                                }
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                                if event_tx
                                    .send(ListenEvent::Lagged(project_id.clone(), skipped))
                                    .await
                                    .is_err()
                                {
                                    break;
                                }
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        }
                    }
                }));
            }
        }
        Ok(tasks)
    }
}

#[tool_handler]
impl ServerHandler for McplsServer {
    async fn list_tools(
        &self,
        request: Option<PaginatedRequestParams>,
        context: rmcp::service::RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let supports_cache_hints = supports_cache_hints(&context);
        let (tools, next_cursor) = advertised_tools_page(
            request
                .as_ref()
                .and_then(|request| request.cursor.as_deref()),
        )
        .map_err(|error| McpError::invalid_params(error, None))?;
        let mut result = ListToolsResult::with_all_items(tools);
        result.next_cursor = next_cursor;
        if supports_cache_hints {
            result.ttl_ms = Some(0);
            result.cache_scope = Some(CacheScope::Public);
        }
        Ok(result)
    }

    async fn call_tool(
        &self,
        request: rmcp::model::CallToolRequestParams,
        context: rmcp::service::RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        self.context.set_client_capabilities(
            context.client_capabilities(),
            context
                .protocol_version()
                .is_some_and(|version| version >= ProtocolVersion::V_2026_07_28),
        );
        let tool_context = ToolCallContext::new(self, request, context);
        Self::tool_router().call(tool_context).await
    }

    async fn list_resources(
        &self,
        request: Option<rmcp::model::PaginatedRequestParams>,
        context: rmcp::service::RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        let open_documents = self
            .context
            .project_registry
            .open_document_paths()
            .await
            .map_err(|error| McpError::internal_error(error.to_string(), None))?;
        let mut resources: Vec<_> = open_documents
            .into_iter()
            .filter_map(|(project_id, path)| {
                let uri = make_uri(&path)
                    .inspect_err(|e| {
                        tracing::warn!(
                            "Skipping path in list_resources (make_uri failed): {}: {e}",
                            path.display()
                        );
                    })
                    .ok()?;
                let name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("unknown");
                Some(
                    Resource::new(uri, format!("{project_id}:{name}"))
                        .with_mime_type("application/json")
                        .with_description(format!("LSP diagnostics for {project_id}:{name}")),
                )
            })
            .collect();

        let projects = self.context.project_registry.list().await;
        resources.extend(projects.iter().map(|identity| {
            let project_id = identity.id().clone();
            let uri = project_status_resource_uri(&project_id);
            Resource::new(uri, format!("{project_id}:status"))
                .with_mime_type("application/json")
                .with_description(format!("Lifecycle status for project {project_id}"))
        }));
        resources.extend(projects.iter().map(|identity| {
            let project_id = identity.id().clone();
            let uri = project_events_resource_uri(&project_id);
            Resource::new(uri, format!("{project_id}:events"))
                .with_mime_type("application/json")
                .with_description(format!("Ordered project events for {project_id}"))
        }));

        let (resources, next_cursor) = resource_page(
            resources,
            request
                .as_ref()
                .and_then(|request| request.cursor.as_deref()),
        )
        .map_err(|error| McpError::invalid_params(error, None))?;
        let mut result = ListResourcesResult::with_all_items(resources);
        result.next_cursor = next_cursor;
        if supports_cache_hints(&context) {
            Ok(result.with_ttl_ms(0).with_cache_scope(CacheScope::Private))
        } else {
            Ok(result)
        }
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        context: rmcp::service::RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, McpError> {
        let resource = parse_session_resource_uri(&request.uri)
            .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
        let supports_cache_hints = supports_cache_hints(&context);
        let path = match resource {
            SessionResource::ProjectStatus(project_id) => {
                return self
                    .read_project_status_resource(project_id, request.uri, supports_cache_hints)
                    .await;
            }
            SessionResource::ProjectEvents { project_id, cursor } => {
                return self
                    .read_project_events_resource(
                        project_id,
                        cursor,
                        request.uri,
                        supports_cache_hints,
                    )
                    .await;
            }
            SessionResource::ProjectEvent {
                project_id,
                sequence,
            } => {
                return self
                    .read_project_event_resource(
                        project_id,
                        sequence,
                        request.uri,
                        supports_cache_hints,
                    )
                    .await;
            }
            SessionResource::EditDiff {
                project_id,
                plan_id,
                offset_bytes,
            } => {
                return self
                    .read_edit_diff_resource(
                        project_id,
                        plan_id,
                        offset_bytes,
                        request.uri,
                        supports_cache_hints,
                    )
                    .await;
            }
            SessionResource::AppliedEditResult {
                project_id,
                plan_id,
                offset_bytes,
            } => {
                return self
                    .read_applied_edit_result_resource(
                        project_id,
                        plan_id,
                        offset_bytes,
                        request.uri,
                        supports_cache_hints,
                    )
                    .await;
            }
            SessionResource::EditApproval {
                project_id,
                plan_id,
                offset_bytes,
            } => {
                return self
                    .read_edit_approval_resource(
                        project_id,
                        plan_id,
                        offset_bytes,
                        request.uri,
                        supports_cache_hints,
                    )
                    .await;
            }
            SessionResource::Diagnostics(path) => path,
            SessionResource::Source(source) => {
                return self
                    .read_source_resource(source, request.uri, supports_cache_hints)
                    .await;
            }
            SessionResource::Deferred(deferred) => {
                return self.read_deferred_resource(deferred, request.uri, supports_cache_hints);
            }
        };

        // TODO(critic-S2): distinguish "file not tracked" from "file tracked but clean"
        // in the response shape. Currently both return `{"diagnostics":null}` which is
        // ambiguous for clients that need to know whether analysis has run yet.
        let actor = self
            .context
            .required_actor_for_path(&path)
            .await
            .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
        actor
            .validate_path(path.display().to_string())
            .await
            .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
        let diagnostics = actor
            .cached_diagnostics(path.display().to_string())
            .await
            .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
        let json = serde_json::to_string(&diagnostics)
            .map_err(|e| McpError::internal_error(format!("Serialization error: {e}"), None))?;

        Ok(private_resource_result(
            vec![ResourceContents::text(json, request.uri)],
            supports_cache_hints,
        )
        .into())
    }

    fn accepted_subscription_filter(
        &self,
        requested: &SubscriptionFilter,
    ) -> Option<SubscriptionFilter> {
        let resources = requested.resource_subscriptions.as_ref()?;
        let accepted = resources
            .iter()
            .filter(|uri| parse_session_resource_uri(uri).is_ok())
            .cloned()
            .collect::<Vec<_>>();
        (!accepted.is_empty()).then(|| {
            SubscriptionFilter::builder()
                .resource_subscriptions(accepted)
                .build()
        })
    }

    async fn listen(&self, context: SubscriptionContext) -> Result<(), McpError> {
        let accepted = context
            .accepted()
            .resource_subscriptions
            .clone()
            .unwrap_or_default();
        let accepted = std::sync::Arc::new(
            accepted
                .into_iter()
                .collect::<std::collections::HashSet<_>>(),
        );
        let project_ids = self.listen_project_ids(&accepted).await?;

        // The event source is live-only. Notify each accepted resource once so clients
        // deterministically re-read the authoritative cached resource on subscription.
        for uri in accepted.iter() {
            if !send_listen_update(&context, uri.clone()).await {
                return Ok(());
            }
        }

        let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(32);
        let tasks = self.spawn_listen_events(project_ids, &event_tx).await?;

        'listen: loop {
            tokio::select! {
                () = context.cancelled() => break,
                queued = event_rx.recv() => {
                    let Some(queued) = queued else { break };
                    let uris = match queued {
                        ListenEvent::Event(project_id, event) => {
                            event_resource_uris(&project_id, &event)
                        }
                        ListenEvent::Lagged(project_id, skipped) => {
                            tracing::warn!(%project_id, skipped, "subscriptions/listen event source lagged");
                            vec![project_events_resource_uri(&project_id)]
                        }
                    };
                    for uri in uris {
                        if accepted.contains(&uri) && !send_listen_update(&context, uri).await {
                            break 'listen;
                        }
                    }
                }
            }
        }
        for task in tasks {
            task.abort();
        }
        Ok(())
    }

    async fn subscribe(
        &self,
        request: SubscribeRequestParams,
        context: rmcp::service::RequestContext<RoleServer>,
    ) -> Result<(), McpError> {
        let resource = parse_session_resource_uri(&request.uri)
            .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
        let path = match resource {
            SessionResource::ProjectStatus(project_id) => {
                self.attach_project_subscription(project_id, request.uri, context.peer)
                    .await?;
                return Ok(());
            }
            SessionResource::ProjectEvents { project_id, .. } => {
                self.attach_project_subscription(
                    project_id.clone(),
                    project_events_resource_uri(&project_id),
                    context.peer,
                )
                .await?;
                return Ok(());
            }
            SessionResource::ProjectEvent { project_id, .. } => {
                self.attach_project_subscription(
                    project_id.clone(),
                    project_events_resource_uri(&project_id),
                    context.peer,
                )
                .await?;
                return Ok(());
            }
            SessionResource::EditDiff { .. } => {
                return Err(McpError::invalid_params(
                    "edit diff resources are readable but not subscribable",
                    None,
                ));
            }
            SessionResource::AppliedEditResult { .. } => {
                return Err(McpError::invalid_params(
                    "applied edit result resources are readable but not subscribable",
                    None,
                ));
            }
            SessionResource::EditApproval { .. } => {
                return Err(McpError::invalid_params(
                    "edit approval resources are readable but not subscribable",
                    None,
                ));
            }
            SessionResource::Diagnostics(path) => path,
            SessionResource::Source(source) => source.path,
            SessionResource::Deferred(_) => {
                return Err(McpError::invalid_params(
                    "deferred semantic resources are readable but not subscribable",
                    None,
                ));
            }
        };

        let (project_id, actor) = self
            .context
            .required_project_for_path(&path)
            .await
            .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
        let has_cached_diagnostics = actor
            .has_cached_diagnostics(path.display().to_string())
            .await
            .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
        self.attach_subscription(
            project_id,
            std::slice::from_ref(&actor),
            request.uri.clone(),
            context.peer.clone(),
        )
        .await?;

        if has_cached_diagnostics {
            context
                .peer
                .notify_resource_updated(ResourceUpdatedNotificationParam::new(request.uri))
                .await
                .map_err(|_| {
                    McpError::internal_error("failed to replay cached diagnostics", None)
                })?;
        }

        Ok(())
    }

    async fn unsubscribe(
        &self,
        request: UnsubscribeRequestParams,
        _context: rmcp::service::RequestContext<RoleServer>,
    ) -> Result<(), McpError> {
        // Parse the URI for consistency with subscribe validation.
        let resource = parse_session_resource_uri(&request.uri)
            .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
        let uri = match resource {
            SessionResource::ProjectEvents { project_id, .. } => {
                project_events_resource_uri(&project_id)
            }
            SessionResource::ProjectEvent { project_id, .. } => {
                project_events_resource_uri(&project_id)
            }
            SessionResource::EditDiff { .. }
            | SessionResource::AppliedEditResult { .. }
            | SessionResource::EditApproval { .. } => request.uri,
            SessionResource::ProjectStatus(_)
            | SessionResource::Diagnostics(_)
            | SessionResource::Source(_)
            | SessionResource::Deferred(_) => request.uri,
        };
        self.context.subscriptions.unsubscribe(&uri).await;
        self.context.event_sink.untrack_subscription(&uri);
        Ok(())
    }

    fn get_info(&self) -> ServerInfo {
        let mut implementation = Implementation::new("mcpls", env!("CARGO_PKG_VERSION"));
        implementation.title = Some("MCPLS - MCP to LSP Bridge".to_string());
        implementation.description = Some(env!("CARGO_PKG_DESCRIPTION").to_string());
        implementation.website_url = Some("https://github.com/bug-ops/mcpls".to_string());

        let capabilities = ServerCapabilities::builder()
            .enable_tools()
            .enable_resources()
            .enable_resources_subscribe()
            .build();
        let mut server_info = ServerInfo::new(capabilities);
        server_info.server_info = implementation;
        server_info.instructions = Some(
            include_str!("server_instructions.txt")
                .trim_end()
                .to_owned(),
        );

        server_info
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::fmt::Write as _;

    use super::*;
    use crate::bridge::resources::parse_uri;
    use crate::edit_plan::{EditPlan, FileSnapshot, SnapshotSource};
    use crate::project::ProjectStatus;
    use tempfile::TempDir;

    fn create_test_server() -> McplsServer {
        let subscriptions = Arc::new(ResourceSubscriptions::new());
        McplsServer::new(subscriptions)
    }

    fn numbered_lines(prefix: &str, count: usize) -> String {
        (0..count).fold(String::new(), |mut output, line| {
            writeln!(output, "{prefix} {line}").unwrap();
            output
        })
    }

    async fn read_all_semantic_json(server: &McplsServer, mut uri: String) -> serde_json::Value {
        let mut encoded = String::new();
        loop {
            let page = server
                .read_semantic_resource(Parameters(SemanticResourceReadParams { uri }))
                .await
                .unwrap();
            let page: serde_json::Value = serde_json::from_str(&page).unwrap();
            assert!(serde_json::to_vec(&page).unwrap().len() <= 16 * 1024);
            encoded.push_str(page["text"].as_str().unwrap());
            let Some(next_uri) = page["next_uri"].as_str() else {
                break;
            };
            uri = next_uri.to_owned();
        }
        serde_json::from_str(&encoded).unwrap()
    }

    async fn assert_structural_source_replays_and_stale_plan_conflicts(
        server: &McplsServer,
        file: &Path,
        source_uri: String,
        original_hash: &serde_json::Value,
        applied: String,
    ) {
        let refreshed = server
            .read_semantic_resource(Parameters(SemanticResourceReadParams { uri: source_uri }))
            .await
            .unwrap();
        let refreshed: serde_json::Value = serde_json::from_str(&refreshed).unwrap();
        let refreshed: serde_json::Value =
            serde_json::from_str(refreshed["text"].as_str().unwrap()).unwrap();
        assert_ne!(&refreshed["content_hash"], original_hash);
        assert!(refreshed["text"].as_str().unwrap().contains("bar("));

        let stale_preview: serde_json::Value = serde_json::from_str(
            &server
                .structural_replace_preview(Parameters(StructuralReplacePreviewParams {
                    project_id: "project".to_owned(),
                    file_path: file.display().to_string(),
                    dialect: "ast_grep".to_owned(),
                    query: "bar($A)".to_owned(),
                    replacement: Some("baz($A)".to_owned()),
                    language_id: Some("rust".to_owned()),
                    parse_only: false,
                    position_encoding: None,
                }))
                .await
                .unwrap(),
        )
        .unwrap();
        let mut externally_changed = applied;
        externally_changed.push_str("// external change\n");
        std::fs::write(file, externally_changed).unwrap();
        let conflict: serde_json::Value = serde_json::from_str(
            &server
                .workspace_edit_apply(Parameters(WorkspaceEditApplyParams {
                    project_id: "project".to_owned(),
                    plan_id: stale_preview["plan_id"].as_str().unwrap().to_owned(),
                    wait_timeout_ms: None,
                }))
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(conflict["status"], "conflict");
        let unchanged = std::fs::read_to_string(file).unwrap();
        assert!(unchanged.contains("// external change"));
        assert!(!unchanged.contains("baz("));
    }

    #[test]
    fn truncated_project_event_pages_expose_a_direct_continuation_uri() {
        let mut history = crate::project::ProjectEventHistory::new(2);
        for generation in 1..=2 {
            history.record(ProjectEvent::ServerExited { generation });
        }
        let project_id = ProjectId::new("project").unwrap();
        let payload = project_events_json(&project_id, &history.snapshot_since(None, 1));

        assert_eq!(
            payload["next_uri"],
            "mcpls-project-events:///project?since=1"
        );
    }

    #[test]
    fn oversized_project_event_body_is_referenced_without_losing_identity() {
        let mut history = crate::project::ProjectEventHistory::new(1);
        history.record(ProjectEvent::StatusChanged {
            status: ProjectStatus::Failed,
            last_error: Some("x".repeat(1024)),
        });
        let project_id = ProjectId::new("project").unwrap();
        let payload = project_events_json(&project_id, &history.snapshot_since(None, 1));

        assert_eq!(payload["events"][0]["sequence"], 1);
        assert_eq!(payload["events"][0]["event"]["kind"], "status_changed");
        assert_eq!(
            payload["events"][0]["resource"]["uri"],
            "mcpls-project-event:///project?sequence=1"
        );
        assert!(
            payload["events"][0]["resource"]["total_bytes"]
                .as_u64()
                .unwrap()
                > 1024
        );
        assert!(payload["events"][0]["event"].get("last_error").is_none());
    }

    #[tokio::test]
    async fn retained_project_event_resources_are_readable_until_evicted() {
        let root = TempDir::new().unwrap();
        let registry = ProjectRegistry::new(1);
        let project_id = ProjectId::new("project").unwrap();
        let actor = registry
            .add(ProjectIdentity::new(
                project_id.clone(),
                CanonicalRoot::new(root.path()).unwrap(),
            ))
            .await
            .unwrap();
        actor.set_status(ProjectStatus::Ready).await.unwrap();

        let server =
            McplsServer::new_with_registry(Arc::new(ResourceSubscriptions::new()), registry);
        let uri = project_event_resource_uri(&project_id, 1);
        let response = server
            .read_project_event_resource(project_id.clone(), 1, uri, false)
            .await
            .unwrap();
        let ReadResourceResponse::Complete(response) = response else {
            panic!("project event resource unexpectedly requested input");
        };
        let ResourceContents::TextResourceContents { text, .. } = &response.contents[0] else {
            panic!("project event resource was not text");
        };
        let event: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(event["sequence"], 1);
        assert_eq!(event["event"]["kind"], "status_changed");

        let error = server
            .read_project_event_resource(
                project_id,
                99,
                "mcpls-project-event:///project?sequence=99".to_owned(),
                false,
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("stale_resource:"));
    }

    #[test]
    fn deferred_resource_pages_are_bounded_and_lossless() {
        const PAGE_LIMIT: usize = 16 * 1024;
        let value = serde_json::json!({
            "references": ["λ\"".repeat(MAX_SEMANTIC_RESOURCE_RESULT_BYTES)],
        });
        let expected = serde_json::to_string(&value).unwrap();
        let mut offset_bytes = 0;
        let mut actual = String::new();

        loop {
            let deferred = DeferredResource {
                token: "token".to_owned(),
                offset_bytes,
            };
            let result = deferred_resource_page(
                &deferred,
                format!("mcpls-deferred:///token?offset_bytes={offset_bytes}"),
                value.clone(),
                "snapshot",
            )
            .unwrap();
            assert!(serde_json::to_vec(&result).unwrap().len() <= PAGE_LIMIT);
            assert_eq!(result.total_bytes, Some(expected.len()));
            assert_eq!(result.offset_bytes, Some(offset_bytes));
            assert_eq!(result.returned_bytes, Some(result.text.len()));
            assert_eq!(result.snapshot_hash.as_deref(), Some("snapshot"));
            assert_eq!(
                result.remaining_bytes,
                Some(expected.len() - offset_bytes - result.text.len())
            );
            actual.push_str(&result.text);
            let Some(next_uri) = result.next_uri else {
                break;
            };
            let SessionResource::Deferred(next) = parse_session_resource_uri(&next_uri).unwrap()
            else {
                panic!("continuation must remain a deferred resource");
            };
            offset_bytes = next.offset_bytes;
        }

        assert_eq!(actual, expected);
    }

    #[tokio::test]
    async fn semantic_resource_tool_pages_an_escaped_source_frame_to_its_outer_budget() {
        const PAGE_LIMIT: usize = 16 * 1024;
        let root = TempDir::new().unwrap();
        let source = root.path().join("quoted.rs");
        let content = format!(
            "{}\ntail sentinel\n",
            "λ\"".repeat(MAX_SEMANTIC_RESOURCE_RESULT_BYTES / 3 + 1)
        );
        std::fs::write(&source, &content).unwrap();
        let registry = ProjectRegistry::new(1);
        registry
            .add(ProjectIdentity::new(
                ProjectId::new("project").unwrap(),
                CanonicalRoot::new(root.path()).unwrap(),
            ))
            .await
            .unwrap();
        let server =
            McplsServer::new_with_registry(Arc::new(ResourceSubscriptions::new()), registry);
        let resource = crate::bridge::resources::SourceResource {
            path: source,
            start_line: 1,
            start_character: 1,
            end_line: 2,
            end_character: 1,
            snapshot_hash: format!("{:x}", Sha256::digest(content.as_bytes())),
            document_version: None,
            offset_bytes: 0,
        };

        let mut result = server
            .read_source_resource_as_tool_result(resource)
            .await
            .unwrap();
        let mut recovered = String::new();
        for _ in 0..16 {
            assert!(serde_json::to_vec(&result).unwrap().len() <= PAGE_LIMIT);
            let frame: crate::bridge::SourceFrame = serde_json::from_str(&result.text).unwrap();
            recovered.push_str(&frame.text);
            if !frame.truncated {
                break;
            }
            let next =
                crate::bridge::resources::parse_source_uri(&frame.resource.unwrap().uri).unwrap();
            result = server
                .read_source_resource_as_tool_result(next)
                .await
                .unwrap();
        }

        assert!(recovered.contains("tail sentinel"));
    }

    #[test]
    fn server_constructor_does_not_require_a_global_translator() {
        let subscriptions = Arc::new(ResourceSubscriptions::new());
        let _server = McplsServer::new(subscriptions);
    }

    #[test]
    fn resource_cache_hints_are_private_and_protocol_gated() {
        let modern = serde_json::to_value(private_resource_result(Vec::new(), true)).unwrap();
        assert_eq!(modern["ttlMs"], 0);
        assert_eq!(modern["cacheScope"], "private");

        let legacy = serde_json::to_value(private_resource_result(Vec::new(), false)).unwrap();
        assert!(legacy.get("ttlMs").is_none());
        assert!(legacy.get("cacheScope").is_none());
    }

    #[test]
    fn listen_filter_accepts_only_session_resource_uris() {
        let server = create_test_server();
        let requested = SubscriptionFilter::builder()
            .resource_subscriptions([
                "lsp-diagnostics:///tmp/main.rs",
                "mcpls-project-status:///project",
                "https://outside.example/resource",
            ])
            .build();
        let accepted = server.accepted_subscription_filter(&requested).unwrap();
        assert_eq!(
            accepted.resource_subscriptions,
            Some(vec![
                "lsp-diagnostics:///tmp/main.rs".to_owned(),
                "mcpls-project-status:///project".to_owned(),
            ])
        );
    }

    #[test]
    fn truncated_preview_keeps_complete_metadata() {
        let original = numbered_lines("old line", 20_000);
        let planned = numbered_lines("new line", 20_000);
        let artifact = PreviewArtifact {
            plan: EditPlan::new(
                "project".to_string(),
                vec![FileSnapshot::from_contents(
                    PathBuf::from("src/huge.rs"),
                    SnapshotSource::Disk,
                    None,
                    original,
                    planned,
                )],
                vec!["text src/huge.rs".to_string()],
                false,
                std::time::Duration::from_secs(60),
            ),
            affected_files: vec![PathBuf::from("src/huge.rs")],
            conflicts: vec!["conflict".to_string()],
            unsupported: vec!["unsupported".to_string()],
            verification: None,
            producer: None,
        };

        let value = preview_artifact_json(&artifact, "project");

        let serialized_bytes = serde_json::to_vec(&value).unwrap().len();
        assert!(
            serialized_bytes <= 16 * 1024,
            "preview used {serialized_bytes} serialized bytes"
        );
        assert_eq!(value["diff_truncated"], true);
        assert_eq!(value["diff_files"][0]["additions"], 20_000);
        assert_eq!(value["diff_files"][0]["deletions"], 20_000);
        assert_eq!(value["operations"][0], "text src/huge.rs");
        assert_eq!(value["conflicts"][0], "conflict");
        assert_eq!(value["unsupported"][0], "unsupported");
        assert_eq!(value["preconditions"].as_array().unwrap().len(), 1);
        assert_eq!(value["safe_to_apply"], false);
        assert_eq!(
            value["diff_resource"]["uri"],
            format!(
                "mcpls-edit-diff:///project?plan_id={}&offset_bytes=0",
                artifact.plan.id().as_str()
            )
        );
    }

    #[test]
    fn applied_edit_result_bounds_inline_detail_and_links_the_complete_receipt() {
        let plan_id = PlanId::parse("plan").unwrap();
        let result = workspace_edit_apply_result(
            ApplyEditPlanOutcome::Applied(AppliedEditPlan {
                plan_id: plan_id.clone(),
                operations: (0..=MAX_INLINE_APPLIED_ITEMS)
                    .map(|index| {
                        format!(
                            "edit src/{index}-{}.rs",
                            "o".repeat(MAX_APPROVAL_TEXT_BYTES + 1)
                        )
                    })
                    .collect(),
                unified_diff: "x".repeat(MAX_INLINE_APPLIED_DIFF_BYTES + 1),
                complete_unified_diff: "complete diff".to_owned(),
                committed_files: (0..=MAX_INLINE_APPLIED_ITEMS)
                    .map(|index| {
                        PathBuf::from(format!(
                            "/workspace/src/{index}-{}.rs",
                            "p".repeat(MAX_APPROVAL_TEXT_BYTES + 1)
                        ))
                    })
                    .collect(),
                verification: None,
                provider_synchronization: (0..=MAX_INLINE_APPLIED_ITEMS)
                    .map(|index| crate::bridge::ProviderSynchronization {
                        provider: format!(
                            "provider-{index}-{}",
                            "n".repeat(MAX_APPROVAL_TEXT_BYTES + 1)
                        ),
                        synchronized: false,
                        watched_file_notifications: index,
                        message: Some("m".repeat(MAX_APPROVAL_TEXT_BYTES + 1)),
                    })
                    .collect(),
            }),
            "project",
            &[PathBuf::from("/workspace")],
        );
        let serialized_bytes = serde_json::to_vec(&result).unwrap().len();
        assert!(
            serialized_bytes <= 16 * 1024,
            "applied result used {serialized_bytes} serialized bytes"
        );

        let WorkspaceEditApplyResult::Applied {
            committed_files,
            committed_file_count,
            operations,
            operation_count,
            detail_resource,
            details_truncated,
            provider_synchronization,
            provider_synchronization_count,
            ..
        } = result
        else {
            panic!("expected an applied edit result");
        };
        assert_eq!(committed_file_count, MAX_INLINE_APPLIED_ITEMS + 1);
        assert_eq!(committed_files.len(), MAX_INLINE_APPLIED_ITEMS);
        assert_eq!(operation_count, MAX_INLINE_APPLIED_ITEMS + 1);
        assert_eq!(operations.len(), MAX_INLINE_APPLIED_ITEMS);
        assert_eq!(provider_synchronization_count, MAX_INLINE_APPLIED_ITEMS + 1);
        assert_eq!(provider_synchronization.len(), MAX_INLINE_APPLIED_ITEMS);
        assert!(details_truncated);
        assert_eq!(
            detail_resource,
            Some(applied_edit_result_resource_uri(
                &ProjectId::new("project").unwrap(),
                &plan_id,
                0,
            ))
        );
    }

    #[test]
    fn applied_edit_result_defers_one_oversized_string() {
        let plan_id = PlanId::parse("plan").unwrap();
        let result = workspace_edit_apply_result(
            ApplyEditPlanOutcome::Applied(AppliedEditPlan {
                plan_id: plan_id.clone(),
                operations: vec!["o".repeat(MAX_APPROVAL_TEXT_BYTES + 1)],
                unified_diff: "small diff".to_owned(),
                complete_unified_diff: "small diff".to_owned(),
                committed_files: vec![PathBuf::from("/workspace/src/lib.rs")],
                verification: None,
                provider_synchronization: vec![crate::bridge::ProviderSynchronization {
                    provider: "p".repeat(MAX_APPROVAL_TEXT_BYTES + 1),
                    synchronized: false,
                    watched_file_notifications: 0,
                    message: Some("m".repeat(MAX_APPROVAL_TEXT_BYTES + 1)),
                }],
            }),
            "project",
            &[PathBuf::from("/workspace")],
        );

        let WorkspaceEditApplyResult::Applied {
            operations,
            provider_synchronization,
            detail_resource,
            details_truncated,
            ..
        } = result
        else {
            panic!("expected an applied edit result");
        };
        assert!(details_truncated);
        assert!(detail_resource.is_some());
        assert!(operations[0].ends_with("... (truncated)"));
        assert!(
            provider_synchronization[0]
                .provider
                .ends_with("... (truncated)")
        );
        assert!(
            provider_synchronization[0]
                .message
                .as_ref()
                .is_some_and(|message| message.ends_with("... (truncated)"))
        );
    }

    #[test]
    fn approval_summary_bounds_inline_lists_and_links_the_complete_plan() {
        let plan_id = PlanId::parse("plan").unwrap();
        let summary = EditPlanApprovalSummary {
            plan_id: plan_id.clone(),
            project_id: "project".to_owned(),
            affected_files: (0..=MAX_APPROVAL_ITEMS)
                .map(|index| PathBuf::from(format!("src/{index}.rs")))
                .collect(),
            operations: (0..=MAX_APPROVAL_ITEMS)
                .map(|index| format!("edit src/{index}.rs"))
                .collect(),
            file_operations: Vec::new(),
            diff_files: Vec::new(),
            diff_truncated: false,
            safe_to_apply: true,
            snapshot_hashes: Vec::new(),
            versions: Vec::new(),
        };

        let value = approval_summary_json(&summary);

        assert_eq!(value["affected_file_count"], MAX_APPROVAL_ITEMS + 1);
        assert_eq!(
            value["affected_files"].as_array().unwrap().len(),
            MAX_APPROVAL_ITEMS
        );
        assert_eq!(value["operation_count"], MAX_APPROVAL_ITEMS + 1);
        assert_eq!(
            value["operations"].as_array().unwrap().len(),
            MAX_APPROVAL_ITEMS
        );
        assert_eq!(value["details_truncated"], true);
        assert_eq!(
            value["detail_resource"]["uri"],
            format!("mcpls-edit-approval:///project?plan_id={plan_id}&offset_bytes=0")
        );
    }

    #[test]
    fn approval_summary_marks_a_truncated_inline_operation() {
        let summary = EditPlanApprovalSummary {
            plan_id: PlanId::parse("plan").unwrap(),
            project_id: "project".to_owned(),
            affected_files: Vec::new(),
            operations: vec!["x".repeat(MAX_APPROVAL_TEXT_BYTES + 1)],
            file_operations: Vec::new(),
            diff_files: Vec::new(),
            diff_truncated: false,
            safe_to_apply: true,
            snapshot_hashes: Vec::new(),
            versions: Vec::new(),
        };

        assert_eq!(approval_summary_json(&summary)["details_truncated"], true);
    }

    #[test]
    fn approval_detail_resource_pages_complete_json() {
        let project_id = ProjectId::new("project").unwrap();
        let plan_id = PlanId::parse("plan").unwrap();
        let detail =
            serde_json::json!({"operations": ["x".repeat(MAX_SEMANTIC_RESOURCE_RESULT_BYTES)]})
                .to_string();
        let mut offset_bytes = 0;
        let mut recovered = String::new();
        for _ in 0..8 {
            let page =
                edit_approval_resource_page(&project_id, &plan_id, offset_bytes, &detail).unwrap();
            assert_eq!(page.mime_type, "application/json");
            assert!(serde_json::to_vec(&page).unwrap().len() <= MAX_SEMANTIC_RESOURCE_RESULT_BYTES);
            recovered.push_str(&page.text);
            let Some(next_uri) = page.next_uri else { break };
            let SessionResource::EditApproval {
                offset_bytes: next, ..
            } = parse_session_resource_uri(&next_uri).unwrap()
            else {
                panic!("continuation must remain an approval resource");
            };
            offset_bytes = next;
        }
        assert_eq!(recovered, detail);
    }

    #[test]
    fn applied_edit_result_resource_pages_complete_json() {
        let project_id = ProjectId::new("project").unwrap();
        let plan_id = PlanId::parse("plan").unwrap();
        let detail =
            serde_json::json!({"operations": ["x".repeat(MAX_SEMANTIC_RESOURCE_RESULT_BYTES)]})
                .to_string();
        let mut offset_bytes = 0;
        let mut recovered = String::new();
        for _ in 0..8 {
            let page =
                applied_edit_result_resource_page(&project_id, &plan_id, offset_bytes, &detail)
                    .unwrap();
            assert_eq!(page.mime_type, "application/json");
            assert!(serde_json::to_vec(&page).unwrap().len() <= MAX_SEMANTIC_RESOURCE_RESULT_BYTES);
            recovered.push_str(&page.text);
            let Some(next_uri) = page.next_uri else {
                break;
            };
            let SessionResource::AppliedEditResult {
                offset_bytes: next, ..
            } = parse_session_resource_uri(&next_uri).unwrap()
            else {
                panic!("continuation must remain an applied edit result resource");
            };
            offset_bytes = next;
        }
        assert_eq!(recovered, detail);
        assert!(serde_json::from_str::<serde_json::Value>(&recovered).is_ok());
    }

    #[tokio::test]
    async fn edit_diff_resource_pages_the_complete_plan_diff_for_its_owner() {
        let root = TempDir::new().unwrap();
        let registry = ProjectRegistry::new(1);
        let project_id = ProjectId::new("project").unwrap();
        let actor = registry
            .add(ProjectIdentity::new(
                project_id.clone(),
                CanonicalRoot::new(root.path()).unwrap(),
            ))
            .await
            .unwrap();
        let plan = EditPlan::new(
            "project".to_owned(),
            vec![FileSnapshot::from_contents(
                root.path().join("huge.rs"),
                SnapshotSource::Disk,
                None,
                numbered_lines("old line", 5_000),
                numbered_lines("new line", 5_000),
            )],
            vec!["text huge.rs".to_owned()],
            true,
            std::time::Duration::from_secs(60),
        );
        let expected = plan.complete_unified_diff();
        assert!(plan.diff_truncated());
        let plan_id = plan.id().clone();
        actor.store_edit_plan(plan).await.unwrap();

        let server =
            McplsServer::new_with_registry(Arc::new(ResourceSubscriptions::new()), registry);
        server.context.remember_plan(plan_id.clone()).await;
        let mut offset_bytes = 0;
        let mut recovered = String::new();
        for _ in 0..32 {
            let uri = edit_diff_resource_uri(&project_id, &plan_id, offset_bytes);
            let response = server
                .read_edit_diff_resource(
                    project_id.clone(),
                    plan_id.clone(),
                    offset_bytes,
                    uri,
                    false,
                )
                .await
                .unwrap();
            let ReadResourceResponse::Complete(response) = response else {
                panic!("edit diff resource unexpectedly requested input");
            };
            let ResourceContents::TextResourceContents { text, .. } = &response.contents[0] else {
                panic!("edit diff resource was not text");
            };
            let page: SemanticResourceReadResult = serde_json::from_str(text).unwrap();
            assert!(serde_json::to_vec(&page).unwrap().len() <= MAX_SEMANTIC_RESOURCE_RESULT_BYTES);
            assert_eq!(page.offset_bytes, Some(offset_bytes));
            recovered.push_str(&page.text);
            let Some(next_uri) = page.next_uri else {
                break;
            };
            let SessionResource::EditDiff {
                offset_bytes: next, ..
            } = parse_session_resource_uri(&next_uri).unwrap()
            else {
                panic!("edit diff continuation was not an edit diff resource");
            };
            offset_bytes = next;
        }
        assert_eq!(recovered, expected);

        let other_session = server.for_session();
        let error = other_session
            .read_edit_diff_resource(
                project_id,
                plan_id,
                0,
                "mcpls-edit-diff:///project?plan_id=unowned&offset_bytes=0".to_owned(),
                false,
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("not owned by this MCP session"));
    }

    #[tokio::test]
    async fn http_session_clones_have_independent_subscriptions() {
        let server = create_test_server();
        let session = server.clone();
        let uri = "lsp-diagnostics:///tmp/session.rs".to_string();

        server
            .context
            .subscriptions
            .subscribe(uri.clone())
            .await
            .unwrap();

        assert!(server.context.subscriptions.contains(&uri).await);
        assert!(!session.context.subscriptions.contains(&uri).await);
    }

    #[tokio::test]
    async fn subscription_list_is_sorted_and_session_local() {
        let server = create_test_server();
        let session = server.for_session();
        server
            .context
            .subscriptions
            .subscribe("lsp-diagnostics:///z.rs".to_string())
            .await
            .unwrap();
        server
            .context
            .subscriptions
            .subscribe("lsp-diagnostics:///a.rs".to_string())
            .await
            .unwrap();

        let result = server
            .subscription_list(Parameters(SubscriptionListParams {}))
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&result).unwrap(),
            serde_json::json!({
                "subscriptions": [
                    "lsp-diagnostics:///a.rs",
                    "lsp-diagnostics:///z.rs"
                ]
            })
        );
        let session_result = session
            .subscription_list(Parameters(SubscriptionListParams {}))
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&session_result).unwrap(),
            serde_json::json!({"subscriptions": []})
        );
    }

    #[tokio::test]
    async fn http_session_clones_share_project_registry() {
        let server = create_test_server();
        let session = server.for_session();
        let root = TempDir::new().unwrap();
        let identity = ProjectIdentity::new(
            ProjectId::new("shared").unwrap(),
            CanonicalRoot::new(root.path()).unwrap(),
        );

        server.context.project_registry.add(identity).await.unwrap();

        assert_eq!(session.context.project_registry.list().await.len(), 1);
    }

    async fn create_test_server_with_project() -> McplsServer {
        let subscriptions = Arc::new(ResourceSubscriptions::new());
        let registry = ProjectRegistry::new(2);
        registry
            .add(ProjectIdentity::new(
                ProjectId::new("project").unwrap(),
                CanonicalRoot::new(".").unwrap(),
            ))
            .await
            .unwrap();
        McplsServer::new_with_registry(subscriptions, registry)
    }

    #[cfg(unix)]
    const CONCURRENCY_LSP: &str = r#"#!/usr/bin/env python3
import json
import fcntl
import os
import pathlib
import sys
import time

counter = pathlib.Path(os.environ["MCPLS_SPAWN_COUNTER"])
active = pathlib.Path(str(counter) + ".active")
max_active = pathlib.Path(str(counter) + ".max-active")
block_root = pathlib.Path(os.environ.get("MCPLS_BLOCK_ROOT", ""))
entered = pathlib.Path(os.environ.get("MCPLS_GATE_ENTERED", ""))
release = pathlib.Path(os.environ.get("MCPLS_GATE_RELEASE", ""))
with counter.open("a+") as counter_file:
    fcntl.flock(counter_file, fcntl.LOCK_EX)
    counter_file.seek(0)
    value = int(counter_file.read() or "0")
    counter_file.seek(0)
    counter_file.truncate()
    counter_file.write(str(value + 1))
    counter_file.flush()
    fcntl.flock(counter_file, fcntl.LOCK_UN)

def adjust_active(delta):
    with active.open("a+") as active_file:
        fcntl.flock(active_file, fcntl.LOCK_EX)
        active_file.seek(0)
        value = int(active_file.read() or "0") + delta
        active_file.seek(0)
        active_file.truncate()
        active_file.write(str(value))
        active_file.flush()
        maximum = int(max_active.read_text() or "0") if max_active.exists() else 0
        if value > maximum:
            max_active.write_text(str(value))
        fcntl.flock(active_file, fcntl.LOCK_UN)

adjust_active(1)

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

def send(message):
    body = json.dumps(message, separators=(",", ":")).encode()
    sys.stdout.buffer.write(
        b"Content-Length: " + str(len(body)).encode() + b"\r\n\r\n" + body
    )
    sys.stdout.buffer.flush()

try:
    while True:
        message = read_message()
        if message is None:
            break
        method = message.get("method")
        if method == "initialize":
            send({"jsonrpc": "2.0", "id": message["id"], "result": {
                "capabilities": {
                    "positionEncoding": "utf-8",
                    "workspaceSymbolProvider": True,
                    "documentSymbolProvider": True
                }
            }})
            send({"jsonrpc": "2.0", "method": "experimental/serverStatus",
                  "params": {"health": "ok", "quiescent": True}})
            send({"jsonrpc": "2.0", "method": "$/progress",
                  "params": {"token": "rustAnalyzer/Indexing", "value": {"kind": "end"}}})
        elif method == "textDocument/documentSymbol":
            if block_root and pathlib.Path.cwd() == block_root:
                entered.write_text("entered")
                while not release.exists():
                    time.sleep(0.001)
            send({"jsonrpc": "2.0", "id": message["id"], "result": []})
        elif method == "workspace/symbol":
            if block_root and pathlib.Path.cwd() == block_root:
                entered.write_text("entered")
                while not release.exists():
                    time.sleep(0.001)
            send({"jsonrpc": "2.0", "id": message["id"], "result": [{
                "name": "fixture_symbol",
                "kind": 12,
                "location": {
                    "uri": "file://" + str(pathlib.Path.cwd() / "src/main.rs"),
                    "range": {
                        "start": {"line": 0, "character": 3},
                        "end": {"line": 0, "character": 7}
                    }
                }
            }]})
        elif method == "shutdown":
            send({"jsonrpc": "2.0", "id": message["id"], "result": None})
            break
finally:
    adjust_active(-1)
"#;

    #[cfg(unix)]
    fn write_concurrency_lsp(
        root: &std::path::Path,
        counter: &std::path::Path,
        block_root: Option<&std::path::Path>,
        entered: Option<&std::path::Path>,
        release: Option<&std::path::Path>,
    ) -> crate::config::LspServerConfig {
        use std::collections::HashMap;
        use std::os::unix::fs::PermissionsExt;

        let script = root.join("concurrency-lsp.py");
        std::fs::write(&script, CONCURRENCY_LSP).unwrap();
        let mut permissions = std::fs::metadata(&script).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&script, permissions).unwrap();

        let mut env = HashMap::from([(
            "MCPLS_SPAWN_COUNTER".to_string(),
            counter.display().to_string(),
        )]);
        if let Some(block_root) = block_root {
            env.insert(
                "MCPLS_BLOCK_ROOT".to_string(),
                block_root.display().to_string(),
            );
        }
        if let Some(entered) = entered {
            env.insert(
                "MCPLS_GATE_ENTERED".to_string(),
                entered.display().to_string(),
            );
        }
        if let Some(release) = release {
            env.insert(
                "MCPLS_GATE_RELEASE".to_string(),
                release.display().to_string(),
            );
        }

        let mut config = crate::config::LspServerConfig::rust_analyzer();
        config.command = script.display().to_string();
        config.env = env;
        config.heuristics = None;
        config
    }

    #[cfg(unix)]
    fn concurrency_template(
        config: crate::config::LspServerConfig,
    ) -> crate::bridge::TranslatorTemplate {
        let mut source = Translator::new().with_extensions(std::collections::HashMap::from([(
            "rs".to_string(),
            "rust".to_string(),
        )]));
        source.set_lsp_configs(vec![config], Some(3));
        source.configuration_template()
    }

    #[cfg(unix)]
    fn write_rust_fixture(root: &std::path::Path) -> std::path::PathBuf {
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname=\"fixture\"\nversion=\"0.1.0\"\nedition=\"2024\"\n",
        )
        .unwrap();
        std::fs::create_dir(root.join("src")).unwrap();
        let file = root.join("src/main.rs");
        std::fs::write(&file, "fn main() {}\nfn fixture_symbol() {}\n").unwrap();
        file
    }

    #[cfg(unix)]
    fn project_params(project_id: &str) -> Parameters<ProjectIdParams> {
        Parameters(ProjectIdParams {
            project_id: project_id.to_string(),
        })
    }

    #[cfg(unix)]
    fn project_add_params(
        project_id: &str,
        root: &std::path::Path,
    ) -> Parameters<ProjectAddParams> {
        Parameters(ProjectAddParams {
            project_id: project_id.to_string(),
            root: root.display().to_string(),
            config: None,
        })
    }

    #[cfg(unix)]
    async fn add_two_projects(
        registry: &ProjectRegistry,
        first_root: &std::path::Path,
        second_root: &std::path::Path,
    ) -> (ProjectId, ProjectId) {
        let first_id = ProjectId::new("first").unwrap();
        let second_id = ProjectId::new("second").unwrap();
        registry
            .add(ProjectIdentity::new(
                first_id.clone(),
                CanonicalRoot::new(first_root).unwrap(),
            ))
            .await
            .unwrap();
        registry
            .add(ProjectIdentity::new(
                second_id.clone(),
                CanonicalRoot::new(second_root).unwrap(),
            ))
            .await
            .unwrap();
        (first_id, second_id)
    }

    #[cfg(unix)]
    fn document_symbols_params(path: &std::path::Path) -> Parameters<DocumentSymbolsParams> {
        Parameters(DocumentSymbolsParams {
            file_path: path.display().to_string(),
            options: DocumentSymbolOptions::default(),
            max_bytes: 16 * 1024,
            page_token: None,
        })
    }

    #[cfg(unix)]
    async fn wait_for_project_ready(registry: &ProjectRegistry, project_id: &ProjectId) {
        let ready = tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                if registry.status(project_id).await.unwrap().status()
                    == crate::project::ProjectStatus::Ready
                {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await;
        assert!(ready.is_ok(), "project did not become ready");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn http_sessions_share_one_project_actor_and_lsp_process() {
        let root = TempDir::new().unwrap();
        let file = write_rust_fixture(root.path());
        let counter = root.path().join("spawn-count");
        let config = write_concurrency_lsp(root.path(), &counter, None, None, None);
        let registry = ProjectRegistry::with_translator_template(4, concurrency_template(config));
        let server =
            McplsServer::new_with_registry(Arc::new(ResourceSubscriptions::new()), registry);
        let first_session = server.for_session();
        let second_session = server.for_session();

        let add_first = first_session.project_add(project_add_params("shared", root.path()));
        let add_second = second_session.project_add(project_add_params("shared", root.path()));
        let (first, second) = tokio::join!(add_first, add_second);
        first.unwrap();
        second.unwrap();

        let activate_first = first_session.project_activate(project_params("shared"));
        let activate_second = second_session.project_activate(project_params("shared"));
        let (first, second) = tokio::join!(activate_first, activate_second);
        first.unwrap();
        second.unwrap();

        let status = first_session
            .project_status(project_params("shared"))
            .await
            .unwrap();
        let status: serde_json::Value = serde_json::from_str(&status).unwrap();
        assert_eq!(status["actor_groups"].as_array().unwrap().len(), 1);
        assert_eq!(std::fs::read_to_string(counter).unwrap(), "1");
        assert!(file.exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn workspace_symbol_search_lazily_activates_the_requested_project() {
        let root = TempDir::new().unwrap();
        write_rust_fixture(root.path());
        let counter = root.path().join("spawn-count");
        let config = write_concurrency_lsp(root.path(), &counter, None, None, None);
        let registry = ProjectRegistry::with_translator_template(4, concurrency_template(config));
        registry
            .add(ProjectIdentity::new(
                ProjectId::new("dormant").unwrap(),
                CanonicalRoot::new(root.path()).unwrap(),
            ))
            .await
            .unwrap();
        let server =
            McplsServer::new_with_registry(Arc::new(ResourceSubscriptions::new()), registry);

        let initial = server
            .workspace_symbol_search(Parameters(WorkspaceSymbolParams {
                project_id: "dormant".to_string(),
                query: Some("fixture".to_string()),
                queries: Vec::new(),
                kind_filter: None,
                match_mode: crate::bridge::WorkspaceSymbolMatchMode::default(),
                scope: crate::bridge::WorkspaceSymbolScope::default(),
                limit: 20,
                max_bytes: 16 * 1024,
                page_token: None,
                include_generated: false,
            }))
            .await;
        assert!(initial.is_err());
        let project_id = ProjectId::new("dormant").unwrap();
        wait_for_project_ready(&server.context.project_registry, &project_id).await;
        let result = server
            .workspace_symbol_search(Parameters(WorkspaceSymbolParams {
                project_id: "dormant".to_string(),
                query: Some("fixture".to_string()),
                queries: Vec::new(),
                kind_filter: None,
                match_mode: crate::bridge::WorkspaceSymbolMatchMode::default(),
                scope: crate::bridge::WorkspaceSymbolScope::default(),
                limit: 20,
                max_bytes: 16 * 1024,
                page_token: None,
                include_generated: false,
            }))
            .await
            .unwrap();
        let result: serde_json::Value = serde_json::from_str(&result).unwrap();

        assert_eq!(result["symbols"][0]["name"], "fixture_symbol");
        assert_eq!(std::fs::read_to_string(counter).unwrap(), "1");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn concurrent_cold_semantic_requests_respect_one_resident_group() {
        let first_root = TempDir::new().unwrap();
        let second_root = TempDir::new().unwrap();
        write_rust_fixture(first_root.path());
        write_rust_fixture(second_root.path());
        let counter = first_root.path().join("spawn-count");
        let config = write_concurrency_lsp(first_root.path(), &counter, None, None, None);
        let registry = ProjectRegistry::with_translator_template(4, concurrency_template(config))
            .with_rust_residency_limit(1)
            .with_rust_residency_idle_timeout(std::time::Duration::ZERO);
        let (first_id, second_id) =
            add_two_projects(&registry, first_root.path(), second_root.path()).await;
        let server =
            McplsServer::new_with_registry(Arc::new(ResourceSubscriptions::new()), registry);

        let (first, second) = tokio::join!(
            server.workspace_symbol_search(Parameters(WorkspaceSymbolParams {
                project_id: first_id.as_str().to_string(),
                query: Some("fixture".to_string()),
                queries: Vec::new(),
                kind_filter: None,
                match_mode: crate::bridge::WorkspaceSymbolMatchMode::default(),
                scope: crate::bridge::WorkspaceSymbolScope::default(),
                limit: 20,
                max_bytes: 16 * 1024,
                page_token: None,
                include_generated: false,
            })),
            server.workspace_symbol_search(Parameters(WorkspaceSymbolParams {
                project_id: second_id.as_str().to_string(),
                query: Some("fixture".to_string()),
                queries: Vec::new(),
                kind_filter: None,
                match_mode: crate::bridge::WorkspaceSymbolMatchMode::default(),
                scope: crate::bridge::WorkspaceSymbolScope::default(),
                limit: 20,
                max_bytes: 16 * 1024,
                page_token: None,
                include_generated: false,
            })),
        );
        assert!(first.is_err());
        assert!(second.is_err());
        assert_eq!(std::fs::read_to_string(&counter).unwrap(), "2");
        assert_eq!(
            std::fs::read_to_string(format!("{}.max-active", counter.display())).unwrap(),
            "1"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn concurrent_activation_and_restart_respect_one_resident_group() {
        let first_root = TempDir::new().unwrap();
        let second_root = TempDir::new().unwrap();
        write_rust_fixture(first_root.path());
        write_rust_fixture(second_root.path());
        let counter = first_root.path().join("spawn-count");
        let config = write_concurrency_lsp(first_root.path(), &counter, None, None, None);
        let registry = ProjectRegistry::with_translator_template(4, concurrency_template(config))
            .with_rust_residency_limit(1)
            .with_rust_residency_idle_timeout(std::time::Duration::ZERO);
        let (first_id, second_id) =
            add_two_projects(&registry, first_root.path(), second_root.path()).await;
        let server =
            McplsServer::new_with_registry(Arc::new(ResourceSubscriptions::new()), registry);

        server
            .project_activate(project_params(first_id.as_str()))
            .await
            .unwrap();
        let (restart, activate) = tokio::join!(
            server.project_restart_lsp(project_params(first_id.as_str())),
            server.project_activate(project_params(second_id.as_str())),
        );
        restart.unwrap();
        activate.unwrap();

        assert_eq!(std::fs::read_to_string(&counter).unwrap(), "3");
        assert_eq!(
            std::fs::read_to_string(format!("{}.max-active", counter.display())).unwrap(),
            "1"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn in_flight_request_blocks_second_semantic_request_until_it_can_resume() {
        let first_root = TempDir::new().unwrap();
        let second_root = TempDir::new().unwrap();
        let first_file = write_rust_fixture(first_root.path());
        write_rust_fixture(second_root.path());
        let counter = first_root.path().join("spawn-count");
        let entered = first_root.path().join("request-entered");
        let release = first_root.path().join("request-release");
        let config = write_concurrency_lsp(
            first_root.path(),
            &counter,
            Some(first_root.path()),
            Some(&entered),
            Some(&release),
        );
        let registry = ProjectRegistry::with_translator_template(4, concurrency_template(config))
            .with_rust_residency_limit(1);
        let (first_id, second_id) =
            add_two_projects(&registry, first_root.path(), second_root.path()).await;
        let server =
            McplsServer::new_with_registry(Arc::new(ResourceSubscriptions::new()), registry);

        server
            .project_activate(project_params(first_id.as_str()))
            .await
            .unwrap();
        wait_for_project_ready(&server.context.project_registry, &first_id).await;

        let blocked_server = server.for_session();
        let blocked = tokio::spawn(async move {
            blocked_server
                .get_document_symbols(document_symbols_params(&first_file))
                .await
        });
        let entered_result = tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while !entered.exists() {
                tokio::task::yield_now().await;
            }
        })
        .await;
        assert!(
            entered_result.is_ok(),
            "in-flight request did not reach the language server"
        );

        let second_server = server.for_session();
        let mut second_request = tokio::spawn(async move {
            second_server
                .workspace_symbol_search(Parameters(WorkspaceSymbolParams {
                    project_id: second_id.as_str().to_string(),
                    query: Some("fixture".to_string()),
                    queries: Vec::new(),
                    kind_filter: None,
                    match_mode: crate::bridge::WorkspaceSymbolMatchMode::default(),
                    scope: crate::bridge::WorkspaceSymbolScope::default(),
                    limit: 20,
                    max_bytes: 16 * 1024,
                    page_token: None,
                    include_generated: false,
                }))
                .await
        });
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), &mut second_request)
                .await
                .is_err(),
            "second cold request bypassed the pinned in-flight group"
        );

        std::fs::write(&release, "release").unwrap();
        blocked.await.unwrap().unwrap();
        let second_result =
            tokio::time::timeout(std::time::Duration::from_secs(3), &mut second_request)
                .await
                .expect("second semantic request should resume after the first request completes")
                .unwrap();
        assert!(second_result.is_err());
        assert_eq!(std::fs::read_to_string(&counter).unwrap(), "2");
        assert_eq!(
            std::fs::read_to_string(format!("{}.max-active", counter.display())).unwrap(),
            "1"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn project_status_and_actor_recover_when_workspace_symbol_caller_cancels() {
        let root = TempDir::new().unwrap();
        write_rust_fixture(root.path());
        let counter = root.path().join("spawn-count");
        let entered = root.path().join("request-entered");
        let release = root.path().join("request-release");
        let config = write_concurrency_lsp(
            root.path(),
            &counter,
            Some(root.path()),
            Some(&entered),
            Some(&release),
        );
        let registry = ProjectRegistry::with_translator_template(4, concurrency_template(config));
        let project_id = ProjectId::new("blocked").unwrap();
        registry
            .add(ProjectIdentity::new(
                project_id.clone(),
                CanonicalRoot::new(root.path()).unwrap(),
            ))
            .await
            .unwrap();
        let server = McplsServer::new_with_registry(
            Arc::new(ResourceSubscriptions::new()),
            registry.clone(),
        );
        server
            .project_activate(project_params(project_id.as_str()))
            .await
            .unwrap();
        wait_for_project_ready(&registry, &project_id).await;

        let blocked_server = server.for_session();
        let blocked_id = project_id.as_str().to_string();
        let blocked = tokio::spawn(async move {
            blocked_server
                .workspace_symbol_search(Parameters(WorkspaceSymbolParams {
                    project_id: blocked_id,
                    query: Some("fixture".to_string()),
                    queries: Vec::new(),
                    kind_filter: None,
                    match_mode: crate::bridge::WorkspaceSymbolMatchMode::default(),
                    scope: crate::bridge::WorkspaceSymbolScope::default(),
                    limit: 20,
                    max_bytes: 16 * 1024,
                    page_token: None,
                    include_generated: false,
                }))
                .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while !entered.exists() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("workspace-symbol request did not reach the language server");

        let status = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            server.project_status(project_params(project_id.as_str())),
        )
        .await;
        blocked.abort();
        let _ = blocked.await;
        let refresh = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            server.project_refresh(project_params(project_id.as_str())),
        )
        .await;
        std::fs::write(&release, "release").unwrap();

        assert!(status.is_ok(), "project status waited behind actor work");
        assert!(
            refresh.is_ok(),
            "cancelled workspace-symbol work held the actor"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rust_residency_evicts_before_spawn_and_resumes_on_semantic_request() {
        let first_root = TempDir::new().unwrap();
        let second_root = TempDir::new().unwrap();
        let first_file = write_rust_fixture(first_root.path());
        write_rust_fixture(second_root.path());
        let counter = first_root.path().join("spawn-count");
        let config = write_concurrency_lsp(first_root.path(), &counter, None, None, None);
        let registry = ProjectRegistry::with_translator_template(4, concurrency_template(config))
            .with_rust_residency_idle_timeout(std::time::Duration::ZERO);
        let (first_id, second_id) =
            add_two_projects(&registry, first_root.path(), second_root.path()).await;
        let server =
            McplsServer::new_with_registry(Arc::new(ResourceSubscriptions::new()), registry);

        server
            .project_activate(project_params(first_id.as_str()))
            .await
            .unwrap();
        server
            .project_activate(project_params(second_id.as_str()))
            .await
            .unwrap();

        assert_eq!(
            server
                .context
                .project_registry
                .status(&first_id)
                .await
                .unwrap()
                .status(),
            crate::project::ProjectStatus::Dormant
        );
        assert_eq!(std::fs::read_to_string(&counter).unwrap(), "2");
        assert_eq!(
            std::fs::read_to_string(format!("{}.max-active", counter.display())).unwrap(),
            "1"
        );

        let result = match server
            .get_document_symbols(document_symbols_params(&first_file))
            .await
        {
            Ok(result) => result,
            Err(error) => {
                assert!(error.message.contains("still initializing"));
                wait_for_project_ready(&server.context.project_registry, &first_id).await;
                server
                    .get_document_symbols(document_symbols_params(&first_file))
                    .await
                    .unwrap()
            }
        };
        let result: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(result["symbols"], serde_json::json!([]));
        assert_eq!(std::fs::read_to_string(&counter).unwrap(), "3");
        assert_eq!(
            server
                .context
                .project_registry
                .status(&second_id)
                .await
                .unwrap()
                .status(),
            crate::project::ProjectStatus::Dormant
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn blocked_project_does_not_delay_other_project_and_removal_keeps_it_ready() {
        let first_root = TempDir::new().unwrap();
        let second_root = TempDir::new().unwrap();
        write_rust_fixture(first_root.path());
        write_rust_fixture(second_root.path());
        let counter = first_root.path().join("spawn-count");
        let entered = first_root.path().join("request-entered");
        let release = first_root.path().join("request-release");
        let config = write_concurrency_lsp(
            first_root.path(),
            &counter,
            Some(first_root.path()),
            Some(&entered),
            Some(&release),
        );
        let registry = ProjectRegistry::with_translator_template(4, concurrency_template(config))
            .with_rust_residency_limit(2);
        let first_id = ProjectId::new("first").unwrap();
        let second_id = ProjectId::new("second").unwrap();
        registry
            .add(ProjectIdentity::new(
                first_id.clone(),
                CanonicalRoot::new(first_root.path()).unwrap(),
            ))
            .await
            .unwrap();
        registry
            .add(ProjectIdentity::new(
                second_id.clone(),
                CanonicalRoot::new(second_root.path()).unwrap(),
            ))
            .await
            .unwrap();
        let server =
            McplsServer::new_with_registry(Arc::new(ResourceSubscriptions::new()), registry);
        let (first_activation, second_activation) = tokio::join!(
            server.project_activate(Parameters(ProjectIdParams {
                project_id: first_id.as_str().to_string(),
            })),
            server.project_activate(Parameters(ProjectIdParams {
                project_id: second_id.as_str().to_string(),
            })),
        );
        first_activation.unwrap();
        second_activation.unwrap();
        wait_for_project_ready(&server.context.project_registry, &first_id).await;
        wait_for_project_ready(&server.context.project_registry, &second_id).await;

        let blocked_server = server.for_session();
        let first_file = first_root.path().join("src/main.rs");
        let blocked = tokio::spawn(async move {
            blocked_server
                .get_document_symbols(document_symbols_params(&first_file))
                .await
        });
        let entered_gate = tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while !entered.exists() {
                tokio::task::yield_now().await;
            }
        })
        .await;
        assert!(
            entered_gate.is_ok(),
            "blocked request did not reach the language server"
        );

        let second_file = second_root.path().join("src/main.rs");
        let second_result = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            server.get_document_symbols(document_symbols_params(&second_file)),
        )
        .await;
        assert!(
            second_result.is_ok(),
            "independent project request timed out"
        );
        let second_result = second_result.unwrap().unwrap();
        let second_result: serde_json::Value = serde_json::from_str(&second_result).unwrap();
        assert_eq!(second_result["symbols"], serde_json::json!([]));

        std::fs::write(&release, "release").unwrap();
        blocked.await.unwrap().unwrap();

        server
            .project_remove(project_params(first_id.as_str()))
            .await
            .unwrap();
        let second_status = server
            .project_status(project_params(second_id.as_str()))
            .await
            .unwrap();
        let second_status: serde_json::Value = serde_json::from_str(&second_status).unwrap();
        assert_eq!(second_status["status"], "Ready");
        assert_eq!(std::fs::read_to_string(counter).unwrap(), "2");
    }

    #[tokio::test]
    async fn test_server_info() {
        let server = create_test_server();
        let info = server.get_info();

        assert!(info.capabilities.tools.is_some());
        assert_eq!(info.server_info.name, "mcpls");
        assert!(info.instructions.is_some());
    }

    #[test]
    fn server_instructions_prefer_source_rich_handle_workflows() {
        let instructions = create_test_server().get_info().instructions.unwrap();

        assert!(
            instructions.len() <= 320,
            "initialize instructions are {} bytes",
            instructions.len()
        );

        for required in [
            "MCPLS-first",
            "workspace_symbol_search",
            "lexical_search",
            "inspect_symbol",
            "ast-grep/SSR",
            "source frames",
            "symbol_handle",
            "stale handles",
            "snapshot resources",
            "preview/apply",
            "project_id",
            "attach/wake",
            "configured skill files directly",
            "no registration/activation",
        ] {
            assert!(
                instructions.contains(required),
                "initialize instructions omit {required}: {instructions}"
            );
        }
        assert!(
            !instructions.contains("project_add") && !instructions.contains("project_activate"),
            "lifecycle setup must be implicit: {instructions}"
        );
        assert!(
            instructions.contains("attach/wake"),
            "lifecycle guidance must describe implicit attach/wake: {instructions}"
        );
    }

    #[test]
    fn initialize_instructions_snapshot_is_current() {
        let snapshot = include_str!("server_instructions.txt").trim_end();
        assert_eq!(
            create_test_server().get_info().instructions.as_deref(),
            Some(snapshot)
        );
    }

    #[test]
    fn semantic_tool_descriptions_explain_source_and_handle_reuse() {
        let tools = McplsServer::tool_router().list_all();
        for name in [
            "workspace_symbol_search",
            "inspect_symbol",
            "inspect_symbol_batch",
            "get_hover",
            "get_definition",
            "get_references",
            "prepare_call_hierarchy",
        ] {
            let description = tools
                .iter()
                .find(|tool| tool.name == name)
                .and_then(|tool| tool.description.as_deref())
                .unwrap_or_default();
            assert!(description.contains("source"), "{name}: {description}");
            assert!(
                description.contains("handle") || name == "get_references",
                "{name}: {description}"
            );
        }
    }

    #[test]
    fn no_reread_guidance_cases_choose_semantic_tools_before_file_reads() {
        let path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../benchmarks/no-reread-guidance.json");
        let cases: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        let cases = cases.as_array().unwrap();

        for case in &cases[..4] {
            let tools = case["expected_tools"].as_array().unwrap();
            assert!(
                matches!(
                    tools[0].as_str(),
                    Some("workspace_symbol_search" | "inspect_symbol")
                ),
                "source question starts with a read: {case}"
            );
            assert!(
                case["forbidden_before_semantic_result"]
                    .as_array()
                    .is_some_and(|forbidden| forbidden.iter().any(|tool| tool == "read_file")),
                "source case permits an early file read: {case}"
            );
        }
        for case in &cases[4..] {
            assert_eq!(case["expected_tools"][0], "read_file");
        }
    }

    #[test]
    fn bundled_skill_and_tool_reference_share_the_no_reread_workflow() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let skill = std::fs::read_to_string(root.join("skills/mcpls/SKILL.md")).unwrap();
        let tools_reference =
            std::fs::read_to_string(root.join("docs/user-guide/tools-reference.md")).unwrap();
        for (path, guidance) in [
            root.join("skills/mcpls/SKILL.md"),
            root.join("docs/user-guide/tools-reference.md"),
        ]
        .into_iter()
        .zip([skill.as_str(), tools_reference.as_str()])
        {
            for required in [
                "workspace_symbol_search",
                "inspect_symbol",
                "lexical_search",
                "symbol_handle",
                "stale_symbol_handle",
                "uncapped full file",
                "Configured Codex skill files are an explicit exception",
                "broaden the project guard",
            ] {
                assert!(
                    guidance.contains(required),
                    "{} omits {required}",
                    path.display()
                );
            }
        }
        assert!(skill.contains("Registered projects need no setup call"));
        assert!(tools_reference.contains("registration or activation round trip"));
    }

    #[test]
    fn json_tools_advertise_output_schemas() {
        let tools = McplsServer::tool_router().list_all();
        let missing: Vec<_> = tools
            .iter()
            .filter(|tool| tool.output_schema.is_none())
            .map(|tool| tool.name.clone().into_owned())
            .collect();

        assert!(
            missing.is_empty(),
            "tools without output schemas: {missing:?}"
        );
        let untyped: Vec<_> = tools
            .iter()
            .filter(|tool| {
                tool.output_schema
                    .as_ref()
                    .is_some_and(|schema| schema.len() == 1)
            })
            .map(|tool| tool.name.clone().into_owned())
            .collect();
        assert!(
            untyped.is_empty(),
            "tools with empty output schemas: {untyped:?}"
        );
        let diagnostics = tools
            .iter()
            .find(|tool| tool.name == "get_diagnostics")
            .unwrap();
        assert!(
            diagnostics.output_schema.as_ref().unwrap()["properties"]["diagnostics"].is_object()
        );
    }

    #[test]
    fn advertised_tools_compact_expanded_output_schemas() {
        let mut full = McplsServer::tool_router().list_all();
        full.sort_by(|left, right| {
            tool_catalog_rank(left.name.as_ref()).cmp(&tool_catalog_rank(right.name.as_ref()))
        });
        let advertised = advertised_tools();
        let full_bytes = serde_json::to_vec(&full).unwrap().len();
        let advertised_bytes = serde_json::to_vec(&advertised).unwrap().len();

        assert_eq!(
            full.iter()
                .filter(|tool| !LEGACY_COMPATIBILITY_TOOLS.contains(&tool.name.as_ref()))
                .map(|tool| &tool.name)
                .collect::<Vec<_>>(),
            advertised.iter().map(|tool| &tool.name).collect::<Vec<_>>()
        );
        assert!(
            advertised_bytes * 2 < full_bytes,
            "advertised tool surface is {advertised_bytes} bytes vs {full_bytes} internally"
        );

        assert!(advertised.iter().all(|tool| tool.output_schema.is_none()));
        for advertised in &advertised {
            let full = full
                .iter()
                .find(|tool| tool.name == advertised.name)
                .unwrap();
            assert_eq!(
                advertised.input_schema.get("type"),
                full.input_schema.get("type")
            );
            assert_eq!(
                advertised.input_schema.get("required"),
                full.input_schema.get("required")
            );
            assert_eq!(
                advertised
                    .input_schema
                    .get("properties")
                    .and_then(Value::as_object)
                    .map(|properties| properties.keys().collect::<Vec<_>>()),
                full.input_schema
                    .get("properties")
                    .and_then(Value::as_object)
                    .map(|properties| properties.keys().collect::<Vec<_>>())
            );
        }
    }

    #[test]
    fn advertised_tools_hide_legacy_direct_mutations_but_keep_compatibility_routes() {
        let router_tools = McplsServer::tool_router().list_all();
        let advertised = advertised_tools();

        for name in LEGACY_COMPATIBILITY_TOOLS {
            assert!(
                router_tools.iter().any(|tool| tool.name == *name),
                "legacy compatibility route is missing {name}"
            );
            assert!(
                !advertised.iter().any(|tool| tool.name == *name),
                "legacy direct mutation is still advertised {name}"
            );
        }
    }

    #[test]
    fn advertised_tools_consolidate_read_only_operation_families() {
        let router_tools = McplsServer::tool_router().list_all();
        let advertised = advertised_tools();
        let mut before = router_tools
            .iter()
            .filter(|tool| {
                ![
                    "rename_symbol",
                    "format_document",
                    "get_code_actions",
                    "get_cached_diagnostics",
                ]
                .contains(&tool.name.as_ref())
            })
            .cloned()
            .map(|mut tool| {
                compact_advertised_input_schema(Arc::make_mut(&mut tool.input_schema));
                tool.description = Some(compact_advertised_description(tool.name.as_ref()).into());
                tool.output_schema = None;
                tool
            })
            .collect::<Vec<_>>();
        before.sort_by(|left, right| {
            tool_catalog_rank(left.name.as_ref()).cmp(&tool_catalog_rank(right.name.as_ref()))
        });

        for name in [
            "workspace_symbol_search_batch",
            "inspect_symbol_batch",
            "get_cached_diagnostics",
            "range_format_preview",
        ] {
            assert!(
                router_tools.iter().any(|tool| tool.name == name),
                "legacy compatibility route is missing {name}"
            );
            assert!(
                !advertised.iter().any(|tool| tool.name == name),
                "legacy compatibility route is still advertised {name}"
            );
        }

        for name in [
            "workspace_symbol_search",
            "inspect_symbol",
            "get_diagnostics",
            "format_preview",
        ] {
            assert!(
                advertised.iter().any(|tool| tool.name == name),
                "canonical tool is missing {name}"
            );
        }
        assert!(
            serde_json::to_vec(&advertised).unwrap().len()
                < serde_json::to_vec(&before).unwrap().len(),
            "canonical tools/list catalog must be smaller than the compatibility catalog"
        );
    }

    #[test]
    fn advertised_tool_pages_are_lossless_and_bounded() {
        let full = advertised_tools();
        let full_bytes = serde_json::to_vec(&full).unwrap().len();
        for name in ["project_list", "project_add", "project_activate"] {
            assert!(
                full.iter().any(|tool| tool.name == name),
                "catalog pagination dropped lifecycle tool {name}"
            );
        }
        let (first_page, first_cursor) = advertised_tools_page(None).unwrap();
        assert!(first_cursor.is_some());
        assert_eq!(
            first_page
                .iter()
                .map(|tool| tool.name.as_ref())
                .collect::<Vec<_>>(),
            [
                "workspace_symbol_search",
                "inspect_symbol",
                "get_diagnostics",
                "format_preview",
                "lexical_search",
                "read_semantic_resource",
                "structural_replace_preview",
                "workspace_edit_preview",
                "workspace_edit_apply",
                "code_action_apply",
                "project_list",
                "code_action_list",
            ]
        );
        assert!(
            full_bytes <= 32 * 1024,
            "full advertised catalog is {full_bytes} bytes"
        );
        assert!(
            serde_json::to_vec(&first_page).unwrap().len() * 2 < full_bytes,
            "default tools/list page must be less than half the full catalog"
        );
        let mut cursor = None;
        let mut names = Vec::new();

        loop {
            let (page, next_cursor) = advertised_tools_page(cursor.as_deref()).unwrap();
            assert!(page.len() <= ADVERTISED_TOOL_PAGE_SIZE);
            names.extend(page.into_iter().map(|tool| tool.name.into_owned()));
            let Some(next_cursor) = next_cursor else {
                break;
            };
            cursor = Some(next_cursor);
        }

        assert_eq!(
            names,
            full.into_iter()
                .map(|tool| tool.name.into_owned())
                .collect::<Vec<_>>()
        );
        assert!(advertised_tools_page(Some("not-an-offset")).is_err());
        assert!(advertised_tools_page(Some(&names.len().to_string())).is_err());
    }
    #[test]
    fn advertised_catalog_fits_native_registration_budget() {
        let tools = advertised_tools();
        assert!(
            native_catalog_bytes(&tools) <= NATIVE_TOOL_CATALOG_MAX_BYTES,
            "native catalog is {} bytes",
            native_catalog_bytes(&tools)
        );
        assert!(tools.iter().any(|tool| tool.name == "lexical_search"));
    }

    #[test]
    fn resource_pages_are_lossless_and_bounded() {
        let resources = || {
            (0..=RESOURCE_PAGE_SIZE)
                .map(|index| Resource::new(format!("mcpls://resource/{index}"), index.to_string()))
                .collect::<Vec<_>>()
        };
        let expected = resources()
            .into_iter()
            .map(|resource| resource.uri)
            .collect::<Vec<_>>();
        let mut cursor = None;
        let mut uris = Vec::new();

        loop {
            let (page, next_cursor) = resource_page(resources(), cursor.as_deref()).unwrap();
            assert!(page.len() <= RESOURCE_PAGE_SIZE);
            uris.extend(page.into_iter().map(|resource| resource.uri));
            let Some(next_cursor) = next_cursor else {
                break;
            };
            cursor = Some(next_cursor);
        }

        assert_eq!(uris, expected);
        assert!(resource_page(resources(), Some("not-an-offset")).is_err());
        assert!(resource_page(resources(), Some(&expected.len().to_string())).is_err());
    }

    #[test]
    fn canonical_symbol_tools_advertise_one_or_many_typed_contracts() {
        let tools = McplsServer::tool_router().list_all();
        let workspace = tools
            .iter()
            .find(|tool| tool.name == "workspace_symbol_search")
            .unwrap();
        let inspect = tools.iter().find(|tool| tool.name == "inspect_symbol");
        assert!(
            inspect.is_some(),
            "inspect_symbol is missing from tools/list"
        );
        let inspect = inspect.unwrap();
        assert!(inspect.input_schema["properties"]["project_id"].is_object());
        assert_eq!(
            workspace.input_schema["properties"]["scope"]["oneOf"][0]["const"],
            "project"
        );
        assert_eq!(
            inspect.input_schema["properties"]["sections"]["items"]["oneOf"][4]["const"],
            "references"
        );
        assert!(workspace.input_schema["properties"]["query"].is_object());
        assert!(workspace.input_schema["properties"]["queries"]["items"].is_object());
        assert!(inspect.input_schema["properties"]["targets"]["items"].is_object());
        assert!(inspect.input_schema["properties"]["budget"]["properties"].is_object());
        assert_eq!(inspect.input_schema["additionalProperties"], false);
        assert_eq!(
            inspect.input_schema["properties"]["budget"]["additionalProperties"],
            false
        );
        assert!(
            inspect
                .output_schema
                .as_ref()
                .is_some_and(|schema| schema["anyOf"].is_array())
        );
        assert!(
            workspace
                .output_schema
                .as_ref()
                .is_some_and(|schema| schema["anyOf"].is_array())
        );
    }

    #[test]
    fn lexical_search_is_advertised_with_explicit_matching_controls() {
        let tool = McplsServer::tool_router()
            .list_all()
            .into_iter()
            .find(|tool| tool.name == "lexical_search")
            .expect("lexical_search is missing from tools/list");

        let schema = serde_json::to_string(&tool.input_schema).unwrap();
        assert!(schema.contains("literal"));
        assert!(schema.contains("regex"));
        assert!(schema.contains("smart"));
        assert!(tool.input_schema["properties"]["max_matches"].is_object());
        assert!(tool.input_schema["properties"]["max_bytes"].is_object());
        assert!(tool.input_schema["properties"]["query"].is_object());
        assert!(tool.input_schema["properties"]["queries"].is_object());
        assert_eq!(
            tool.input_schema["properties"]["max_bytes"]["default"],
            16 * 1024
        );
        assert!(tool.output_schema.as_ref().is_some_and(|schema| {
            schema["anyOf"].is_array()
                && schema["anyOf"]
                    .as_array()
                    .is_some_and(|variants| variants.len() == 2)
        }));
    }

    #[test]
    fn lexical_page_caps_transcript_sized_caller_budgets() {
        assert_eq!(effective_lexical_page_bytes(50_000), 16 * 1024);
        assert_eq!(effective_lexical_page_bytes(6_000), 6_000);

        let matches = (0..100)
            .map(|index| crate::bridge::lexical::LexicalSearchMatch {
                project_relative_path: format!("swift/RideMapState{index:03}.swift"),
                document_version: None,
                content_hash: "a".repeat(64),
                source_uri: format!("mcpls-source:///swift/RideMapState{index:03}.swift"),
                source: None,
                byte_range: 0..1,
            })
            .collect();
        let page =
            bounded_lexical_page(matches, 0, false, effective_lexical_page_bytes(50_000)).unwrap();

        assert_eq!(page.max_bytes, 16 * 1024);
        assert!(serde_json::to_vec(&page).unwrap().len() <= page.max_bytes);
        assert!(page.next_cursor.is_some());
    }

    #[test]
    fn lexical_page_respects_its_serialized_byte_budget() {
        let matches = (0..2)
            .map(|index| crate::bridge::lexical::LexicalSearchMatch {
                project_relative_path: format!("src/{index:0200}.rs"),
                document_version: None,
                content_hash: "a".repeat(64),
                source_uri: format!("mcpls-source:///src/{index:0200}.rs"),
                source: None,
                byte_range: 0..1,
            })
            .collect();

        let page = bounded_lexical_page(matches, 0, false, 800).unwrap();

        assert!(serde_json::to_vec(&page).unwrap().len() <= 800);
        assert_eq!(page.returned, 1);
        assert!(page.truncated);
        assert_eq!(page.next_cursor.as_deref(), Some("1"));
    }

    #[test]
    fn lexical_page_rejects_a_budget_that_cannot_return_one_identity() {
        let matches = vec![crate::bridge::lexical::LexicalSearchMatch {
            project_relative_path: format!("src/{}.rs", "a".repeat(8_000)),
            document_version: None,
            content_hash: "a".repeat(64),
            source_uri: "mcpls-source:///src/too-long.rs".to_owned(),
            source: None,
            byte_range: 0..1,
        }];

        assert!(bounded_lexical_page(matches, 0, false, 4 * 1024).is_err());
    }

    #[tokio::test]
    async fn inspect_symbol_batch_rejects_a_budget_that_cannot_cover_every_target() {
        let server = create_test_server();
        let target = || crate::bridge::InspectSymbolTarget {
            symbol_handle: None,
            query: Some("run".to_owned()),
            kind: None,
            path: None,
            container: None,
        };
        let error = server
            .inspect_symbol_batch(Parameters(InspectSymbolBatchParams {
                project_id: "project".to_owned(),
                targets: vec![target(), target()],
                candidate_limit: 10,
                sections: Vec::new(),
                budget: crate::bridge::InspectSymbolBudget {
                    max_bytes: 16 * 1024,
                    max_items: 1,
                },
                page_token: None,
            }))
            .await
            .unwrap_err();

        assert!(error.message.contains("at least one item per target"));
    }

    #[test]
    fn deferred_semantic_resources_are_readable_as_a_tool() {
        let tools = McplsServer::tool_router().list_all();
        let read = tools
            .iter()
            .find(|tool| tool.name == "read_semantic_resource")
            .expect("deferred references need a callable tool fallback");

        assert!(read.input_schema["properties"]["uri"].is_object());
        assert!(read.output_schema.as_ref().is_some());
    }

    #[test]
    fn project_list_tool_schema_exposes_cursor_pagination() {
        let tools = McplsServer::tool_router().list_all();
        let project_list = tools
            .iter()
            .find(|tool| tool.name == "project_list")
            .unwrap();

        let expected = serde_json::json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "properties": {
                "cursor": {
                    "default": null,
                    "description": "Decimal cursor returned by a prior `project_list` response.",
                    "type": ["string", "null"]
                }
            },
            "type": "object"
        });
        assert_eq!(
            project_list.input_schema.as_ref(),
            expected.as_object().unwrap()
        );
    }

    #[test]
    fn tool_surface_snapshot_is_current() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/mcp/tool_surface.json");
        let rendered =
            serde_json::to_string_pretty(&McplsServer::tool_router().list_all()).unwrap();
        if std::env::var_os("UPDATE_TOOL_SURFACE").is_some() {
            std::fs::write(path, format!("{rendered}\n")).unwrap();
            return;
        }

        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            format!("{rendered}\n")
        );
    }

    #[test]
    fn structured_tool_result_keeps_json_out_of_default_text_content() {
        let output = encode_json(&serde_json::json!({
            "line": 7,
            "available": true,
            "optional": null,
            "truncated": true,
            "items": vec!["large-result-entry"; 1_000],
            "source": { "status": "available" },
        }))
        .unwrap();
        let result = output.into_result(false);

        assert_eq!(
            result.structured_content,
            Some(serde_json::json!({
                "line": 7,
                "available": true,
                "optional": null,
                "truncated": true,
                "items": vec!["large-result-entry"; 1_000],
                "source": { "status": "available" },
            }))
        );
        assert_eq!(
            result.content[0].as_text().unwrap().text,
            "Structured result available in structuredContent."
        );
    }

    #[test]
    fn legacy_text_mode_keeps_the_previous_json_payload() {
        let output = encode_json(&serde_json::json!({"line": 7, "available": true})).unwrap();
        let result = output.into_result(true);

        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&result.content[0].as_text().unwrap().text)
                .unwrap(),
            result.structured_content.unwrap()
        );
    }

    #[test]
    fn operation_errors_include_stable_actionable_data() {
        let error =
            encode_tool_result::<crate::bridge::DiagnosticsResult, _>(Err("offline")).unwrap_err();

        assert_eq!(error.data.as_ref().unwrap()["code"], "operation_failed");
        assert_eq!(error.data.as_ref().unwrap()["retryable"], true);
        assert!(error.data.as_ref().unwrap()["action"].is_string());
    }

    #[tokio::test]
    async fn unknown_project_routing_explains_recovery_without_activation() {
        let server = create_test_server();
        let error = server
            .workspace_symbol_search(Parameters(WorkspaceSymbolParams {
                project_id: "missing".to_owned(),
                query: Some("needle".to_owned()),
                queries: Vec::new(),
                kind_filter: None,
                match_mode: crate::bridge::WorkspaceSymbolMatchMode::default(),
                scope: crate::bridge::WorkspaceSymbolScope::default(),
                limit: 1,
                max_bytes: 4_096,
                page_token: None,
                include_generated: false,
            }))
            .await
            .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("project is not registered: missing")
        );
        let data = error
            .data
            .as_ref()
            .expect("unknown project should be actionable");
        assert_eq!(data["code"], "project_not_registered");
        assert_eq!(data["retryable"], false);
        assert!(data["action"].as_str().unwrap().contains("project_list"));
        assert!(data["action"].as_str().unwrap().contains("project_add"));
    }

    #[tokio::test]
    async fn project_status_reports_dormancy_metadata() {
        let server = create_test_server();
        let root = TempDir::new().unwrap();
        server
            .project_add(Parameters(ProjectAddParams {
                project_id: "dormant".to_string(),
                root: root.path().display().to_string(),
                config: None,
            }))
            .await
            .unwrap();
        let actor = server
            .context
            .project_registry
            .actor_for_project(&ProjectId::new("dormant").unwrap())
            .await
            .unwrap();
        actor
            .set_status(crate::project::ProjectStatus::Dormant)
            .await
            .unwrap();

        let status = server
            .project_status(Parameters(ProjectIdParams {
                project_id: "dormant".to_string(),
            }))
            .await
            .unwrap();
        let status: serde_json::Value = serde_json::from_str(&status).unwrap();
        assert_eq!(status["dormancy"]["reason"], "restored");
        assert!(status["dormancy"]["idle_for_ms"].is_null());
    }

    #[tokio::test]
    async fn test_project_lifecycle_tools_share_registry() {
        let server = create_test_server();
        let root = TempDir::new().unwrap();
        let added = server
            .project_add(Parameters(ProjectAddParams {
                project_id: "demo".to_string(),
                root: root.path().display().to_string(),
                config: None,
            }))
            .await
            .unwrap();
        let added_json: serde_json::Value = serde_json::from_str(&added).unwrap();
        assert_eq!(added_json["project_id"], "demo");
        assert_eq!(added_json["roots"].as_array().unwrap().len(), 1);
        assert_eq!(added_json["actor_group_count"], 1);
        assert_eq!(added_json["generation"], 0);
        assert_eq!(added_json["actor_groups"][0]["group_id"], 0);
        assert_eq!(
            added_json["actor_groups"][0]["roots"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        let capabilities = server
            .project_lsp_capabilities(Parameters(ProjectLspCapabilitiesParams {
                project_id: "demo".to_string(),
                language_id: None,
            }))
            .await
            .unwrap();
        let capabilities_json: serde_json::Value = serde_json::from_str(&capabilities).unwrap();
        assert!(capabilities_json["servers"].is_array());
        let duplicate = server
            .project_add(Parameters(ProjectAddParams {
                project_id: "demo".to_string(),
                root: root.path().display().to_string(),
                config: Some(serde_json::json!({})),
            }))
            .await
            .unwrap();
        assert!(duplicate.contains("demo"));

        let listed = server
            .project_list(Parameters(ProjectListParams::default()))
            .await
            .unwrap();
        assert!(listed.contains("demo"));

        let status = server
            .project_status(Parameters(ProjectIdParams {
                project_id: "demo".to_string(),
            }))
            .await
            .unwrap();
        assert!(status.contains("Starting"));

        let restarted = server
            .project_restart_lsp(Parameters(ProjectIdParams {
                project_id: "demo".to_string(),
            }))
            .await
            .unwrap();
        assert!(restarted.contains("Ready"));
        let refreshed = server
            .project_refresh(Parameters(ProjectIdParams {
                project_id: "demo".to_string(),
            }))
            .await
            .unwrap();
        assert!(refreshed.contains("Ready"));

        server
            .project_remove(Parameters(ProjectIdParams {
                project_id: "demo".to_string(),
            }))
            .await
            .unwrap();
        assert!(
            !server
                .project_list(Parameters(ProjectListParams::default()))
                .await
                .unwrap()
                .contains("demo")
        );
    }

    #[tokio::test]
    async fn project_list_pages_every_registered_project() {
        let parent = TempDir::new().unwrap();
        let registry = ProjectRegistry::new(2);
        for index in 0..33 {
            let root = parent.path().join(index.to_string());
            std::fs::create_dir(&root).unwrap();
            registry
                .add(ProjectIdentity::new(
                    ProjectId::new(format!("project-{index:02}")).unwrap(),
                    CanonicalRoot::new(root).unwrap(),
                ))
                .await
                .unwrap();
        }
        let server =
            McplsServer::new_with_registry(Arc::new(ResourceSubscriptions::new()), registry);

        let first: serde_json::Value = serde_json::from_str(
            &server
                .project_list(Parameters(ProjectListParams::default()))
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(first["returned"], 32);
        assert!(first["truncated"].as_bool().unwrap());
        let cursor = first["next_cursor"].as_str().unwrap().to_owned();
        let second: serde_json::Value = serde_json::from_str(
            &server
                .project_list(Parameters(ProjectListParams {
                    cursor: Some(cursor),
                }))
                .await
                .unwrap(),
        )
        .unwrap();

        let ids = first["projects"]
            .as_array()
            .unwrap()
            .iter()
            .chain(second["projects"].as_array().unwrap())
            .map(|project| project["project_id"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(ids.len(), 33);
        assert_eq!(ids.first(), Some(&"project-00"));
        assert_eq!(ids.last(), Some(&"project-32"));
        assert_eq!(second["returned"], 1);
        assert!(!second["truncated"].as_bool().unwrap());
        assert!(second["next_cursor"].is_null());
    }

    #[tokio::test]
    async fn project_add_applies_project_lsp_configuration() {
        let root = TempDir::new().unwrap();
        std::fs::write(
            root.path().join("Cargo.toml"),
            "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        )
        .unwrap();
        let server = create_test_server();

        let added = server
            .project_add(Parameters(ProjectAddParams {
                project_id: "configured".to_string(),
                root: root.path().display().to_string(),
                config: Some(serde_json::json!({
                    "lsp_servers": [{
                        "language_id": "rust",
                        "command": "/definitely/missing/rust-analyzer",
                        "file_patterns": ["**/*.rs"],
                        "heuristics": {"project_markers": ["Cargo.toml"]}
                    }],
                    "heuristics_max_depth": 3
                })),
            }))
            .await
            .unwrap();
        let added: serde_json::Value = serde_json::from_str(&added).unwrap();

        assert_eq!(
            added["configured_language_servers"],
            serde_json::json!(["rust"])
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn workspace_symbol_falls_back_when_project_activation_fails() {
        let root = TempDir::new().unwrap();
        let source = write_rust_fixture(root.path());
        std::fs::write(&source, "fn fixture_symbol() {}\n").unwrap();
        let server = create_test_server();

        server
            .project_add(Parameters(ProjectAddParams {
                project_id: "activation-fallback".to_string(),
                root: root.path().display().to_string(),
                config: Some(serde_json::json!({
                    "lsp_servers": [{
                        "language_id": "rust",
                        "command": "/definitely/missing/rust-analyzer",
                        "file_patterns": ["**/*.rs"],
                        "heuristics": {"project_markers": ["Cargo.toml"]}
                    }]
                })),
            }))
            .await
            .unwrap();

        let result = server
            .workspace_symbol_search(Parameters(WorkspaceSymbolParams {
                project_id: "activation-fallback".to_string(),
                query: Some("fixture_symbol".to_string()),
                queries: Vec::new(),
                kind_filter: None,
                match_mode: crate::bridge::WorkspaceSymbolMatchMode::default(),
                scope: crate::bridge::WorkspaceSymbolScope::default(),
                limit: 20,
                max_bytes: 16 * 1024,
                page_token: None,
                include_generated: false,
            }))
            .await
            .unwrap();
        let result: serde_json::Value = serde_json::from_str(&result).unwrap();

        assert_eq!(result["symbols"][0]["name"], "fixture_symbol");
    }

    #[tokio::test]
    async fn health_and_server_status_use_non_blocking_project_snapshots() {
        let server = create_test_server();
        let health: serde_json::Value = serde_json::from_str(
            &server
                .health(Parameters(DaemonStatusParams {}))
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(health["status"], "healthy");
        assert_eq!(health["lifecycle"], "running");
        assert_eq!(health["persistence"]["configured"], false);
        assert_eq!(health["transport"]["mode"], "stdio");
        assert!(health["transport"]["bind"].is_null());
        assert!(health["transport"]["path"].is_null());
        assert_eq!(health["projects"]["starting"], 0);
        assert_eq!(health["actor_groups"], 0);

        let root = TempDir::new().unwrap();
        server
            .context
            .project_registry
            .add(ProjectIdentity::new(
                ProjectId::new("health").unwrap(),
                CanonicalRoot::new(root.path()).unwrap(),
            ))
            .await
            .unwrap();
        let status: serde_json::Value = serde_json::from_str(
            &server
                .server_status(Parameters(DaemonStatusParams {}))
                .await
                .unwrap(),
        )
        .unwrap();
        assert!(status["uptime_seconds"].is_number());
        assert_eq!(status["lifecycle"], "running");
        assert_eq!(status["persistence"]["configured"], false);
        assert_eq!(status["transport"]["mode"], "stdio");
        assert_eq!(status["session_count"], 0);
        assert_eq!(status["queue_pressure"]["queued"], 0);
        assert_eq!(status["queue_pressure"]["capacity"], 32);
        assert_eq!(status["projects"]["starting"], 1);
        assert_eq!(status["actor_groups"], 1);
        assert_eq!(status["project_summaries"][0]["project_id"], "health");
        assert_eq!(status["project_summaries"][0]["status"], "Starting");
        assert_eq!(status["project_summaries"][0]["actor_group_count"], 1);
        assert_eq!(
            status["project_summaries"][0]["roots"][0].as_str(),
            Some(root.path().to_str().unwrap())
        );

        server.context.project_registry.shutdown_all().await;
        let shutdown_health: serde_json::Value = serde_json::from_str(
            &server
                .health(Parameters(DaemonStatusParams {}))
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(shutdown_health["lifecycle"], "shutting_down");
    }

    #[tokio::test]
    async fn health_distinguishes_failed_projects_from_degraded_state() {
        let server = create_test_server();
        let root = TempDir::new().unwrap();
        let project_id = ProjectId::new("failed").unwrap();
        server
            .context
            .project_registry
            .add(ProjectIdentity::new(
                project_id.clone(),
                CanonicalRoot::new(root.path()).unwrap(),
            ))
            .await
            .unwrap();
        server
            .context
            .project_registry
            .actor_for_project(&project_id)
            .await
            .unwrap()
            .fail("test failure")
            .await
            .unwrap();

        let health: serde_json::Value = serde_json::from_str(
            &server
                .health(Parameters(DaemonStatusParams {}))
                .await
                .unwrap(),
        )
        .unwrap();

        assert_eq!(health["status"], "failed");
        assert_eq!(health["projects"]["failed"], 1);
    }

    #[tokio::test]
    async fn health_reports_persistence_errors() {
        let parent = TempDir::new().unwrap();
        let blocker = parent.path().join("not-a-directory");
        std::fs::write(&blocker, "blocker").unwrap();
        let registry = ProjectRegistry::new(2).with_persistence(
            crate::project_persistence::ProjectRegistrationStore::new(blocker.join("state")),
        );
        let server = McplsServer::new_with_registry(
            Arc::new(ResourceSubscriptions::new()),
            registry.clone(),
        );

        let root = TempDir::new().unwrap();
        let result = registry
            .add(ProjectIdentity::new(
                ProjectId::new("persistence-error").unwrap(),
                CanonicalRoot::new(root.path()).unwrap(),
            ))
            .await;
        assert!(result.is_err());

        let health: serde_json::Value = serde_json::from_str(
            &server
                .health(Parameters(DaemonStatusParams {}))
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(health["status"], "degraded");
        assert!(health["persistence"]["last_error"].is_string());
    }

    #[tokio::test]
    async fn workspace_edit_apply_retries_return_the_committed_receipt() {
        let root = TempDir::new().unwrap();
        let file = root.path().join("src.rs");
        std::fs::write(&file, "before\n").unwrap();

        let registry = ProjectRegistry::new(2);
        let identity = ProjectIdentity::new(
            ProjectId::new("project").unwrap(),
            CanonicalRoot::new(root.path()).unwrap(),
        );
        let actor = registry.add(identity).await.unwrap();
        let plan = EditPlan::new(
            "project".to_string(),
            vec![FileSnapshot::from_contents(
                file,
                SnapshotSource::Disk,
                None,
                "before\n",
                "after\n",
            )],
            vec!["replace src.rs".to_string()],
            true,
            std::time::Duration::from_secs(60),
        );
        let plan_id = plan.id().as_str().to_string();
        actor.store_edit_plan(plan).await.unwrap();

        let subscriptions = Arc::new(ResourceSubscriptions::new());
        let server = McplsServer::new_with_registry(subscriptions, registry);
        server
            .context
            .remember_plan(PlanId::parse(plan_id.clone()).unwrap())
            .await;
        let result = server
            .workspace_edit_apply(Parameters(WorkspaceEditApplyParams {
                project_id: "project".to_string(),
                plan_id: plan_id.clone(),
                wait_timeout_ms: None,
            }))
            .await
            .unwrap();
        let result: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(result["project_id"], "project");
        assert_eq!(result["plan_id"], plan_id);
        assert_eq!(result["committed_files"].as_array().unwrap().len(), 1);
        assert_eq!(
            std::fs::read_to_string(root.path().join("src.rs")).unwrap(),
            "after\n"
        );

        let detail = server
            .read_applied_edit_result_resource(
                ProjectId::new("project").unwrap(),
                PlanId::parse(plan_id.clone()).unwrap(),
                0,
                format!("mcpls-edit-result:///project?plan_id={plan_id}&offset_bytes=0"),
                false,
            )
            .await
            .unwrap();
        let ReadResourceResponse::Complete(detail) = detail else {
            panic!("applied detail resource unexpectedly requested input");
        };
        let ResourceContents::TextResourceContents { text, .. } = &detail.contents[0] else {
            panic!("applied detail resource was not text");
        };
        let page: SemanticResourceReadResult = serde_json::from_str(text).unwrap();
        let detail: serde_json::Value = serde_json::from_str(&page.text).unwrap();
        assert_eq!(detail["plan_id"], plan_id);
        assert_eq!(detail["operations"], serde_json::json!(["replace src.rs"]));
        assert_eq!(detail["unified_diff"], result["unified_diff"]);

        let events = server
            .read_project_events_resource(
                ProjectId::new("project").unwrap(),
                None,
                "mcpls-project-events:///project".to_string(),
                false,
            )
            .await
            .unwrap();
        let ReadResourceResponse::Complete(events) = events else {
            panic!("project events resource unexpectedly requested input");
        };
        let ResourceContents::TextResourceContents {
            text: event_text, ..
        } = &events.contents[0]
        else {
            panic!("project events resource was not text");
        };
        let event_payload: serde_json::Value = serde_json::from_str(event_text).unwrap();
        assert_eq!(event_payload["project_id"], "project");
        assert_eq!(event_payload["resync_required"], false);
        assert_eq!(event_payload["retention_floor"], 0);
        assert_eq!(event_payload["returned_events"], 2);
        assert_eq!(event_payload["first_sequence"], 1);
        assert_eq!(event_payload["last_sequence"], 2);
        assert_eq!(event_payload["next_cursor"], 2);
        assert_eq!(event_payload["events"].as_array().unwrap().len(), 2);
        assert_eq!(event_payload["events"][0]["event"]["kind"], "files_changed");
        assert_eq!(event_payload["events"][1]["event"]["kind"], "edit_applied");

        let second = server
            .workspace_edit_apply(Parameters(WorkspaceEditApplyParams {
                project_id: "project".to_string(),
                plan_id,
                wait_timeout_ms: None,
            }))
            .await
            .unwrap();
        let second: serde_json::Value = serde_json::from_str(&second).unwrap();
        assert_eq!(second, result);
        assert_eq!(
            std::fs::read_to_string(root.path().join("src.rs")).unwrap(),
            "after\n"
        );
    }

    #[tokio::test]
    async fn workspace_edit_apply_reports_competing_session_as_retryable() {
        let root = TempDir::new().unwrap();
        let file = root.path().join("src.rs");
        std::fs::write(&file, "before\n").unwrap();
        let registry = ProjectRegistry::new(2);
        let project_id = ProjectId::new("project").unwrap();
        let actor = registry
            .add(ProjectIdentity::new(
                project_id.clone(),
                CanonicalRoot::new(root.path()).unwrap(),
            ))
            .await
            .unwrap();
        let plan = |after| {
            EditPlan::new(
                project_id.to_string(),
                vec![FileSnapshot::from_contents(
                    file.clone(),
                    SnapshotSource::Disk,
                    None,
                    "before\n",
                    after,
                )],
                vec!["replace src.rs".to_owned()],
                true,
                std::time::Duration::from_secs(60),
            )
            .with_workspace_root(root.path().to_path_buf())
        };
        let first = plan("first\n");
        let second = plan("second\n");
        let first_id = first.id().as_str().to_owned();
        let second_id = second.id().as_str().to_owned();
        actor.store_edit_plan(first).await.unwrap();
        actor.store_edit_plan(second).await.unwrap();

        let server = McplsServer::new_with_registry(
            Arc::new(ResourceSubscriptions::new()),
            registry.clone(),
        );
        let other_session = server.for_session();
        server
            .context
            .remember_plan(PlanId::parse(first_id.clone()).unwrap())
            .await;
        other_session
            .context
            .remember_plan(PlanId::parse(second_id.clone()).unwrap())
            .await;
        let lease = registry.acquire_test_edit_lease(file.clone());

        let busy: serde_json::Value = serde_json::from_str(
            &other_session
                .workspace_edit_apply(Parameters(WorkspaceEditApplyParams {
                    project_id: project_id.to_string(),
                    plan_id: second_id.clone(),
                    wait_timeout_ms: Some(0),
                }))
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(busy["status"], "not_ready");
        assert_eq!(busy["reason"], "edit_in_progress");
        assert_eq!(busy["retry"]["action"], "retry_apply");
        assert_eq!(busy["retry"]["same_plan"], true);
        assert_eq!(busy["contention"]["scope"], "same_worktree");
        assert_eq!(
            busy["contention"]["blocked_paths"],
            serde_json::json!(["src.rs"])
        );

        drop(lease);
        let first: serde_json::Value = serde_json::from_str(
            &server
                .workspace_edit_apply(Parameters(WorkspaceEditApplyParams {
                    project_id: project_id.to_string(),
                    plan_id: first_id,
                    wait_timeout_ms: None,
                }))
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(first["status"], "applied");

        let stale: serde_json::Value = serde_json::from_str(
            &other_session
                .workspace_edit_apply(Parameters(WorkspaceEditApplyParams {
                    project_id: project_id.to_string(),
                    plan_id: second_id,
                    wait_timeout_ms: None,
                }))
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(stale["status"], "conflict");
        assert_eq!(std::fs::read_to_string(file).unwrap(), "first\n");
    }

    #[tokio::test]
    async fn structural_ast_grep_search_preview_and_apply_share_the_session_plan_path() {
        let root = TempDir::new().unwrap();
        let file = root.path().join("src.rs");
        std::fs::write(&file, "fn main() { foo(1); foo(2); }\n").unwrap();
        let registry = ProjectRegistry::new(2);
        registry
            .add(ProjectIdentity::new(
                ProjectId::new("project").unwrap(),
                CanonicalRoot::new(root.path()).unwrap(),
            ))
            .await
            .unwrap();
        let server =
            McplsServer::new_with_registry(Arc::new(ResourceSubscriptions::new()), registry);

        let search: serde_json::Value = serde_json::from_str(
            &server
                .structural_replace_preview(Parameters(StructuralReplacePreviewParams {
                    project_id: "project".to_string(),
                    file_path: file.display().to_string(),
                    dialect: "ast_grep".to_string(),
                    query: "foo($A)".to_string(),
                    replacement: None,
                    language_id: Some("rust".to_string()),
                    parse_only: false,
                    position_encoding: None,
                }))
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(search["engine"], "ast_grep");
        assert_eq!(search["dialect"], "ast_grep");
        assert_eq!(search["match_count"], 2);
        assert!(search.get("plan_id").is_none());

        let no_match: serde_json::Value = serde_json::from_str(
            &server
                .structural_replace_preview(Parameters(StructuralReplacePreviewParams {
                    project_id: "project".to_string(),
                    file_path: file.display().to_string(),
                    dialect: "ast_grep".to_string(),
                    query: "missing($A)".to_string(),
                    replacement: Some("bar($A)".to_string()),
                    language_id: Some("rust".to_string()),
                    parse_only: false,
                    position_encoding: None,
                }))
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(no_match["match_count"], 0);
        assert!(no_match.get("plan_id").is_none());

        let preview: serde_json::Value = serde_json::from_str(
            &server
                .structural_replace_preview(Parameters(StructuralReplacePreviewParams {
                    project_id: "project".to_string(),
                    file_path: file.display().to_string(),
                    dialect: "ast_grep".to_string(),
                    query: "foo($A)".to_string(),
                    replacement: Some("bar($A)".to_string()),
                    language_id: Some("rust".to_string()),
                    parse_only: false,
                    position_encoding: None,
                }))
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(preview["match_count"], 2);
        assert_eq!(preview["producer"], "structural_ast_grep");
        assert_eq!(preview["verification"], "structural_unverified");
        assert_eq!(preview["safe_to_apply"], true);
        let plan_id = preview["plan_id"].as_str().unwrap().to_string();

        server
            .workspace_edit_apply(Parameters(WorkspaceEditApplyParams {
                project_id: "project".to_string(),
                plan_id,
                wait_timeout_ms: None,
            }))
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(file).unwrap(),
            "fn main() { bar(1); bar(2); }\n"
        );
    }

    #[tokio::test]
    async fn oversized_structural_preview_is_bounded_lossless_and_atomic() {
        const MATCH_COUNT: usize = 81;

        let root = TempDir::new().unwrap();
        let file = root.path().join("src.rs");
        let mut source = String::new();
        for index in 0..MATCH_COUNT {
            writeln!(
                source,
                "fn fixture_{index}() {{ foo({index}); }} // {}",
                "padding".repeat(24)
            )
            .unwrap();
        }
        std::fs::write(&file, source).unwrap();
        let registry = ProjectRegistry::new(2);
        registry
            .add(ProjectIdentity::new(
                ProjectId::new("project").unwrap(),
                CanonicalRoot::new(root.path()).unwrap(),
            ))
            .await
            .unwrap();
        let server =
            McplsServer::new_with_registry(Arc::new(ResourceSubscriptions::new()), registry);

        let response = server
            .structural_replace_preview(Parameters(StructuralReplacePreviewParams {
                project_id: "project".to_owned(),
                file_path: file.display().to_string(),
                dialect: "ast_grep".to_owned(),
                query: "foo($A)".to_owned(),
                replacement: Some("bar($A)".to_owned()),
                language_id: Some("rust".to_owned()),
                parse_only: false,
                position_encoding: None,
            }))
            .await
            .unwrap();
        assert!(response.len() <= MAX_SEMANTIC_RESOURCE_RESULT_BYTES);
        let preview: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(preview["match_count"], MATCH_COUNT);
        assert_eq!(
            preview["returned_match_count"].as_u64().unwrap()
                + preview["remaining_match_count"].as_u64().unwrap(),
            MATCH_COUNT as u64
        );

        let matches = read_all_semantic_json(
            &server,
            preview["matches_resource"]["uri"]
                .as_str()
                .unwrap()
                .to_owned(),
        )
        .await;
        assert_eq!(matches["files"].as_array().unwrap().len(), 1);
        assert_eq!(matches["matches"].as_array().unwrap().len(), MATCH_COUNT);
        let unique = matches["matches"]
            .as_array()
            .unwrap()
            .iter()
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(unique.len(), MATCH_COUNT);
        let source_uri = matches["files"][0]["source_resource"]["uri"]
            .as_str()
            .unwrap()
            .to_owned();

        let details = read_all_semantic_json(
            &server,
            preview["plan_details_resource"]["uri"]
                .as_str()
                .unwrap()
                .to_owned(),
        )
        .await;
        assert_eq!(details["preconditions"].as_array().unwrap().len(), 1);
        assert_eq!(
            u64::try_from(details["preconditions"].as_array().unwrap().len()).unwrap(),
            preview["precondition_count"].as_u64().unwrap()
        );
        assert_eq!(
            u64::try_from(details["operations"].as_array().unwrap().len()).unwrap(),
            preview["operation_count"].as_u64().unwrap()
        );

        server
            .workspace_edit_apply(Parameters(WorkspaceEditApplyParams {
                project_id: "project".to_owned(),
                plan_id: preview["plan_id"].as_str().unwrap().to_owned(),
                wait_timeout_ms: None,
            }))
            .await
            .unwrap();
        let applied = std::fs::read_to_string(&file).unwrap();
        assert_eq!(applied.matches("bar(").count(), MATCH_COUNT);
        assert_eq!(applied.matches("foo(").count(), 0);

        assert_structural_source_replays_and_stale_plan_conflicts(
            &server,
            &file,
            source_uri,
            &matches["files"][0]["snapshot_hash"],
            applied,
        )
        .await;
    }

    #[tokio::test]
    async fn oversized_structural_search_lists_every_match_through_its_resource() {
        const MATCH_COUNT: usize = 256;

        let root = TempDir::new().unwrap();
        let file = root.path().join("src.rs");
        let mut source = String::new();
        for index in 0..MATCH_COUNT {
            writeln!(source, "fn fixture_{index}() {{ foo({index}); }}").unwrap();
        }
        std::fs::write(&file, source).unwrap();
        let registry = ProjectRegistry::new(2);
        registry
            .add(ProjectIdentity::new(
                ProjectId::new("project").unwrap(),
                CanonicalRoot::new(root.path()).unwrap(),
            ))
            .await
            .unwrap();
        let server =
            McplsServer::new_with_registry(Arc::new(ResourceSubscriptions::new()), registry);

        let response = server
            .structural_replace_preview(Parameters(StructuralReplacePreviewParams {
                project_id: "project".to_owned(),
                file_path: file.display().to_string(),
                dialect: "ast_grep".to_owned(),
                query: "foo($A)".to_owned(),
                replacement: None,
                language_id: Some("rust".to_owned()),
                parse_only: false,
                position_encoding: None,
            }))
            .await
            .unwrap();
        assert!(response.len() <= 16 * 1024, "{} bytes", response.len());
        let response: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(response["match_count"], MATCH_COUNT);
        assert!(response.get("plan_id").is_none());
        assert!(response.get("plan_details_resource").is_none());

        let inventory = read_all_semantic_json(
            &server,
            response["matches_resource"]["uri"]
                .as_str()
                .unwrap()
                .to_owned(),
        )
        .await;
        assert_eq!(inventory["file_count"], 1);
        assert_eq!(inventory["match_count"], MATCH_COUNT);
        assert_eq!(inventory["matches"].as_array().unwrap().len(), MATCH_COUNT);
    }

    #[tokio::test]
    async fn workspace_edit_preview_returns_plan_for_lsp_workspace_edit() {
        let root = TempDir::new().unwrap();
        let file = root.path().join("src.rs");
        std::fs::write(&file, "before\n").unwrap();

        let registry = ProjectRegistry::new(2);
        let identity = ProjectIdentity::new(
            ProjectId::new("project").unwrap(),
            CanonicalRoot::new(root.path()).unwrap(),
        );
        registry.add(identity).await.unwrap();

        let server =
            McplsServer::new_with_registry(Arc::new(ResourceSubscriptions::new()), registry);
        let uri = url::Url::from_file_path(&file).unwrap().to_string();
        let result = server
            .workspace_edit_preview(Parameters(WorkspaceEditPreviewParams {
                project_id: "project".to_string(),
                workspace_edit: serde_json::json!({
                    "changes": {
                        uri: [{
                            "range": {
                                "start": {"line": 0, "character": 0},
                                "end": {"line": 0, "character": 6}
                            },
                            "newText": "after"
                        }]
                    }
                }),
                position_encoding: Some("utf-8".to_string()),
            }))
            .await
            .unwrap();
        let result: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert!(!result["plan_id"].as_str().unwrap().is_empty());
        assert!(result["unified_diff"].as_str().unwrap().contains("-before"));
        assert_eq!(result["diff_files"][0]["additions"], 1);
        assert_eq!(result["diff_files"][0]["deletions"], 1);
        assert_eq!(result["diff_truncated"], false);
        assert_eq!(result["affected_files"].as_array().unwrap().len(), 1);
        assert_eq!(result["safe_to_apply"], true);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "before\n");

        let other_session = server.for_session();
        let cross_session = other_session
            .workspace_edit_apply(Parameters(WorkspaceEditApplyParams {
                project_id: "project".to_string(),
                plan_id: result["plan_id"].as_str().unwrap().to_string(),
                wait_timeout_ms: None,
            }))
            .await
            .unwrap_err()
            .to_string();
        assert!(cross_session.contains("not owned by this MCP session"));

        server
            .workspace_edit_apply(Parameters(WorkspaceEditApplyParams {
                project_id: "project".to_string(),
                plan_id: result["plan_id"].as_str().unwrap().to_string(),
                wait_timeout_ms: None,
            }))
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(file).unwrap(), "after\n");
    }

    #[tokio::test]
    async fn rename_and_format_preview_require_an_explicit_registered_project() {
        let server = create_test_server();
        let rename = server
            .rename_preview(Parameters(RenamePreviewParams {
                project_id: "missing".to_string(),
                file_path: "/tmp/example.rs".to_string(),
                line: 1,
                character: 1,
                new_name: "renamed".to_string(),
                position_encoding: None,
            }))
            .await
            .unwrap_err()
            .to_string();
        assert!(rename.contains("project is not registered"), "{rename}");

        let format = server
            .format_preview(Parameters(FormatPreviewParams {
                project_id: "missing".to_string(),
                file_path: "/tmp/example.rs".to_string(),
                tab_size: 4,
                insert_spaces: true,
                position_encoding: None,
                range: None,
            }))
            .await
            .unwrap_err()
            .to_string();
        assert!(format.contains("project is not registered"), "{format}");

        let rename_apply = server
            .rename_apply(Parameters(WorkspaceEditApplyParams {
                project_id: "missing".to_string(),
                plan_id: "plan-1".to_string(),
                wait_timeout_ms: None,
            }))
            .await
            .unwrap_err()
            .to_string();
        assert!(
            rename_apply.contains("edit plan is not owned by this MCP session"),
            "{rename_apply}"
        );

        let format_apply = server
            .format_apply(Parameters(WorkspaceEditApplyParams {
                project_id: "missing".to_string(),
                plan_id: "plan-1".to_string(),
                wait_timeout_ms: None,
            }))
            .await
            .unwrap_err()
            .to_string();
        assert!(
            format_apply.contains("edit plan is not owned by this MCP session"),
            "{format_apply}"
        );
    }

    #[cfg(unix)]
    const FAKE_EDIT_LSP: &str = r#"#!/usr/bin/env python3
import json
import os
import pathlib
import sys
import urllib.parse

counter = pathlib.Path(os.environ["MCPLS_COUNTER"])
vfs = {}

def path_for_uri(uri):
    return pathlib.Path(urllib.parse.unquote(urllib.parse.urlparse(uri).path))

def file_text(uri):
    if uri not in vfs:
        vfs[uri] = path_for_uri(uri).read_text()
    return vfs[uri]

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

def send(message):
    body = json.dumps(message, separators=(",", ":")).encode()
    sys.stdout.buffer.write(
        b"Content-Length: " + str(len(body)).encode() + b"\r\n\r\n" + body
    )
    sys.stdout.buffer.flush()

def bump():
    value = int(counter.read_text()) if counter.exists() else 0
    counter.write_text(str(value + 1))

while True:
    message = read_message()
    if message is None:
        break
    method = message.get("method")
    if method == "initialize":
        capabilities = {
            "positionEncoding": "utf-8",
            "renameProvider": True,
            "documentFormattingProvider": True,
            "experimental": {"ssr": True}
        }
        if True:
            capabilities["documentSymbolProvider"] = True
            capabilities["documentRangeFormattingProvider"] = True
            capabilities["declarationProvider"] = True
            capabilities["selectionRangeProvider"] = True
            capabilities["experimental"]["moveItem"] = True
            capabilities["experimental"]["parentModule"] = True
            capabilities["experimental"]["childModules"] = True
            capabilities["experimental"]["runnables"] = {"kinds": ["cargo"]}
        capabilities["workspace"] = {"fileOperations": {"willRename": {"filters": [
            {"scheme": "file", "pattern": {"glob": "**/*.rs", "matches": "file"}}
        ]}}}
        send({"jsonrpc": "2.0", "id": message["id"], "result": {
            "capabilities": capabilities,
            "serverInfo": {"name": "rust-analyzer", "version": "test"}
        }})
        send({"jsonrpc": "2.0", "method": "experimental/serverStatus",
              "params": {"health": "ok", "quiescent": True}})
        send({"jsonrpc": "2.0", "method": "$/progress",
              "params": {"token": "rustAnalyzer/Indexing", "value": {"kind": "end"}}})
        send({"jsonrpc": "2.0", "id": "watch-register", "method": "client/registerCapability",
              "params": {"registrations": [{
                  "id": "rust-files", "method": "workspace/didChangeWatchedFiles",
                  "registerOptions": {"watchers": [
                      {"globPattern": "**/*.rs", "kind": 7}
                  ]}
              }]}})
    elif method == "textDocument/didOpen":
        document = message["params"]["textDocument"]
        vfs[document["uri"]] = document["text"]
    elif method == "textDocument/didChange":
        uri = message["params"]["textDocument"]["uri"]
        vfs[uri] = message["params"]["contentChanges"][-1]["text"]
    elif method == "textDocument/rename":
        bump()
        uri = message["params"]["textDocument"]["uri"]
        sibling_uri = uri.replace("src.rs", "other.rs")
        old_name = file_text(uri).splitlines()[0][:8]
        new_name = message["params"]["newName"]
        changes = {}
        for target in (uri, sibling_uri):
            if file_text(target).startswith(old_name):
                changes[target] = [{
                    "range": {"start": {"line": 0, "character": 0},
                              "end": {"line": 0, "character": 8}},
                    "newText": new_name
                }]
        edit = {"changes": changes}
        send({"jsonrpc": "2.0", "id": message["id"], "result": edit})
    elif method == "textDocument/formatting":
        bump()
        uri = message["params"]["textDocument"]["uri"]
        send({"jsonrpc": "2.0", "id": message["id"], "result": [
            {"range": {"start": {"line": 0, "character": 0},
                        "end": {"line": 0, "character": 8}},
             "newText": "formatted"}
        ]})
    elif method == "textDocument/rangeFormatting":
        uri = message["params"]["textDocument"]["uri"]
        if message["params"]["range"]["start"]["line"] == 97:
            result = []
        else:
            result = [{"range": message["params"]["range"], "newText": "ranged"}]
        send({"jsonrpc": "2.0", "id": message["id"], "result": result})
    elif method == "experimental/moveItem":
        params = message["params"]
        if params["range"]["start"]["line"] == 98:
            result = []
        elif params["direction"] == "Up":
            result = [{"range": params["range"], "newText": "${1:unresolved}",
                       "insertTextFormat": 2}]
        else:
            result = [{"range": params["range"], "newText": "moved\n",
                       "insertTextFormat": 2}]
        send({"jsonrpc": "2.0", "id": message["id"], "result": result})
    elif method == "textDocument/declaration":
        uri = message["params"]["textDocument"]["uri"]
        send({"jsonrpc": "2.0", "id": message["id"], "result": {
            "uri": uri,
            "range": {"start": {"line": 1, "character": 2},
                      "end": {"line": 1, "character": 5}}
        }})
    elif method in ("experimental/parentModule", "experimental/childModules"):
        uri = message["params"]["textDocument"]["uri"]
        line = 2 if method.endswith("parentModule") else 3
        send({"jsonrpc": "2.0", "id": message["id"], "result": [{
            "uri": uri,
            "range": {"start": {"line": line, "character": 0},
                      "end": {"line": line, "character": 4}}
        }]})
    elif method == "rust-analyzer/expandMacro":
        send({"jsonrpc": "2.0", "id": message["id"], "result": {
            "name": "fixture!", "expansion": "fn expanded() {}"
        }})
    elif method == "textDocument/selectionRange":
        send({"jsonrpc": "2.0", "id": message["id"], "result": [{
            "range": {"start": {"line": 0, "character": 2},
                      "end": {"line": 0, "character": 4}},
            "parent": {"range": {"start": {"line": 0, "character": 0},
                                  "end": {"line": 0, "character": 8}}}
        }]})
    elif method == "experimental/runnables":
        send({"jsonrpc": "2.0", "id": message["id"], "result": [{
            "label": "test fixture",
            "kind": "cargo",
            "args": {"environment": {"RUST_BACKTRACE": "1"}, "cwd": "/workspace",
                     "workspaceRoot": "/workspace", "cargoArgs": ["test", "fixture"],
                     "executableArgs": ["--exact"], "overrideCargo": None}
        }]})
    elif method == "rust-analyzer/relatedTests":
        send({"jsonrpc": "2.0", "id": message["id"], "result": [{
            "runnable": {"label": "test related", "kind": "cargo",
                         "args": {"environment": {}, "cwd": "/workspace",
                                  "workspaceRoot": "/workspace",
                                  "cargoArgs": ["test", "related"],
                                  "executableArgs": [], "overrideCargo": None}}
        }]})
    elif method == "textDocument/documentSymbol":
        uri = message["params"]["textDocument"]["uri"]
        path = path_for_uri(uri)
        send({"jsonrpc": "2.0", "id": message["id"],
              "result": [] if path.exists() else None})
    elif method == "rust-analyzer/viewFileText":
        uri = message["params"]["uri"]
        send({"jsonrpc": "2.0", "id": message["id"], "result": file_text(uri)})
    elif method == "workspace/didChangeWatchedFiles":
        if False:
            sys.exit(0)
        for change in message["params"]["changes"]:
            uri = change["uri"]
            path = path_for_uri(uri)
            if path.exists():
                vfs[uri] = path.read_text()
            else:
                vfs.pop(uri, None)
    elif method == "experimental/ssr":
        bump()
        uri = message["params"]["textDocument"]["uri"]
        changes = {} if message["params"]["parseOnly"] else {
            uri: [{"range": {"start": {"line": 0, "character": 0},
                              "end": {"line": 1, "character": 0}},
                   "newText": "structural\n"}]
        }
        send({"jsonrpc": "2.0", "id": message["id"], "result": {"changes": changes}})
    elif method == "workspace/willRenameFiles":
        bump()
        old_uri = message["params"]["files"][0]["oldUri"]
        sibling_uri = old_uri.replace("src.rs", "other.rs")
        send({"jsonrpc": "2.0", "id": message["id"], "result": {"changes": {
            sibling_uri: [{"range": {"start": {"line": 0, "character": 0},
                                      "end": {"line": 0, "character": 8}},
                           "newText": "path_ref"}]
        }}})
    elif method == "shutdown":
        send({"jsonrpc": "2.0", "id": message["id"], "result": None})
        break
"#;

    #[cfg(unix)]
    async fn fake_text_edit_fixture(
        exit_on_watched_files: bool,
    ) -> (TempDir, PathBuf, PathBuf, McplsServer) {
        use std::collections::HashMap;
        use std::fs;
        use std::os::unix::fs::PermissionsExt;

        let root = TempDir::new().unwrap();
        let source = root.path().join("src.rs");
        let sibling = root.path().join("other.rs");
        let counter = root.path().join("request-count");
        fs::write(&source, "old_name\n").unwrap();
        fs::write(&sibling, "old_name();\n").unwrap();

        let lsp = root.path().join("fake-edit-lsp.py");
        let script = if exit_on_watched_files {
            FAKE_EDIT_LSP.replace(
                "if False:\n            sys.exit(0)",
                "if True:\n            sys.exit(0)",
            )
        } else {
            FAKE_EDIT_LSP.to_string()
        };
        fs::write(&lsp, script).unwrap();
        let mut permissions = fs::metadata(&lsp).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&lsp, permissions).unwrap();

        let mut config = crate::config::LspServerConfig::rust_analyzer();
        config.command = lsp.display().to_string();
        config.heuristics = None;
        config.env = HashMap::from([("MCPLS_COUNTER".to_string(), counter.display().to_string())]);
        let mut translator = Translator::new()
            .with_extensions(HashMap::from([("rs".to_string(), "rust".to_string())]));
        translator.set_lsp_configs(vec![config], Some(3));
        let registry =
            ProjectRegistry::with_translator_template(4, translator.configuration_template());
        let project_id = ProjectId::new("fixture").unwrap();
        registry
            .add(ProjectIdentity::new(
                project_id.clone(),
                CanonicalRoot::new(root.path()).unwrap(),
            ))
            .await
            .unwrap();
        registry.activate(&project_id).await.unwrap();
        let server =
            McplsServer::new_with_registry(Arc::new(ResourceSubscriptions::new()), registry);
        (root, source, sibling, server)
    }

    #[cfg(unix)]
    async fn apply_two_file_text_edit(
        server: &McplsServer,
        source: &Path,
        sibling: &Path,
    ) -> serde_json::Value {
        let source_uri = url::Url::from_file_path(source).unwrap().to_string();
        let sibling_uri = url::Url::from_file_path(sibling).unwrap().to_string();
        let preview: serde_json::Value = serde_json::from_str(
            &server
                .workspace_edit_preview(Parameters(WorkspaceEditPreviewParams {
                    project_id: "fixture".to_string(),
                    workspace_edit: serde_json::json!({
                        "changes": {
                            source_uri: [{
                                "range": {
                                    "start": {"line": 0, "character": 0},
                                    "end": {"line": 0, "character": 8}
                                },
                                "newText": "new_name"
                            }],
                            sibling_uri: [{
                                "range": {
                                    "start": {"line": 0, "character": 0},
                                    "end": {"line": 0, "character": 8}
                                },
                                "newText": "new_name"
                            }]
                        }
                    }),
                    position_encoding: Some("utf-8".to_string()),
                }))
                .await
                .unwrap(),
        )
        .unwrap();
        serde_json::from_str(
            &server
                .workspace_edit_apply(Parameters(WorkspaceEditApplyParams {
                    project_id: "fixture".to_string(),
                    plan_id: preview["plan_id"].as_str().unwrap().to_string(),
                    wait_timeout_ms: None,
                }))
                .await
                .unwrap(),
        )
        .unwrap()
    }

    #[cfg(unix)]
    async fn assert_inverse_rename_sees_both_files(server: &McplsServer, source: &Path) {
        let reverse: serde_json::Value = serde_json::from_str(
            &server
                .rename_preview(Parameters(RenamePreviewParams {
                    project_id: "fixture".to_string(),
                    file_path: source.display().to_string(),
                    line: 1,
                    character: 1,
                    new_name: "old_name".to_string(),
                    position_encoding: Some("utf-8".to_string()),
                }))
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(reverse["affected_files"].as_array().unwrap().len(), 2);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn text_only_apply_synchronizes_when_all_files_are_unopened() {
        let (_root, source, sibling, server) = fake_text_edit_fixture(false).await;

        let applied = apply_two_file_text_edit(&server, &source, &sibling).await;

        assert_eq!(applied["semantic_state"], "synchronized", "{applied}");
        assert_inverse_rename_sees_both_files(&server, &source).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn text_only_apply_synchronizes_multiple_tracked_files() {
        let (_root, source, sibling, server) = fake_text_edit_fixture(false).await;
        for path in [&source, &sibling] {
            server
                .get_document_symbols(Parameters(DocumentSymbolsParams {
                    file_path: path.display().to_string(),
                    options: DocumentSymbolOptions::default(),
                    max_bytes: 16 * 1024,
                    page_token: None,
                }))
                .await
                .unwrap();
        }

        let applied = apply_two_file_text_edit(&server, &source, &sibling).await;

        assert_eq!(applied["semantic_state"], "synchronized", "{applied}");
        assert_inverse_rename_sees_both_files(&server, &source).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn provider_exit_during_text_sync_keeps_the_committed_apply_result() {
        let (_root, source, sibling, server) = fake_text_edit_fixture(true).await;

        let applied = apply_two_file_text_edit(&server, &source, &sibling).await;

        assert_eq!(applied["semantic_state"], "degraded", "{applied}");
        assert_eq!(
            applied["provider_synchronization"][0]["synchronized"],
            false
        );
        assert_eq!(std::fs::read_to_string(source).unwrap(), "new_name\n");
        assert_eq!(std::fs::read_to_string(sibling).unwrap(), "new_name();\n");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn text_only_apply_synchronizes_unopened_files_before_follow_up_rename() {
        let (_root, source, _sibling, server) = fake_text_edit_fixture(false).await;
        let project_id = ProjectId::new("fixture").unwrap();

        let rename: serde_json::Value = serde_json::from_str(
            &server
                .rename_preview(Parameters(RenamePreviewParams {
                    project_id: project_id.as_str().to_string(),
                    file_path: source.display().to_string(),
                    line: 1,
                    character: 1,
                    new_name: "new_name".to_string(),
                    position_encoding: Some("utf-8".to_string()),
                }))
                .await
                .unwrap(),
        )
        .unwrap();
        let applied: serde_json::Value = serde_json::from_str(
            &server
                .workspace_edit_apply(Parameters(WorkspaceEditApplyParams {
                    project_id: project_id.as_str().to_string(),
                    plan_id: rename["plan_id"].as_str().unwrap().to_string(),
                    wait_timeout_ms: None,
                }))
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(applied["semantic_state"], "synchronized", "{applied}");
        assert!(
            applied["provider_synchronization"][0]["watched_file_notifications"]
                .as_u64()
                .is_some_and(|count| count > 0),
            "{applied}"
        );

        let reverse: serde_json::Value = serde_json::from_str(
            &server
                .rename_preview(Parameters(RenamePreviewParams {
                    project_id: project_id.as_str().to_string(),
                    file_path: source.display().to_string(),
                    line: 1,
                    character: 1,
                    new_name: "old_name".to_string(),
                    position_encoding: Some("utf-8".to_string()),
                }))
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(reverse["affected_files"].as_array().unwrap().len(), 2);
    }

    #[cfg(unix)]
    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn rename_and_format_wrappers_apply_stored_lsp_plans_without_re_requesting() {
        use std::collections::HashMap;
        use std::fs;
        use std::os::unix::fs::PermissionsExt;

        let root = TempDir::new().unwrap();
        let source = root.path().join("src.rs");
        let sibling = root.path().join("other.rs");
        let counter = root.path().join("request-count");
        fs::write(&source, "old_name\n").unwrap();
        fs::write(&sibling, "old_name();\n").unwrap();

        let lsp = root.path().join("fake-edit-lsp.py");
        fs::write(&lsp, FAKE_EDIT_LSP).unwrap();
        let mut permissions = fs::metadata(&lsp).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&lsp, permissions).unwrap();

        let mut server_config = crate::config::LspServerConfig::rust_analyzer();
        server_config.command = lsp.display().to_string();
        server_config.heuristics = None;
        server_config.env =
            HashMap::from([("MCPLS_COUNTER".to_string(), counter.display().to_string())]);
        let mut template_source = Translator::new()
            .with_extensions(HashMap::from([("rs".to_string(), "rust".to_string())]));
        template_source.set_lsp_configs(vec![server_config], Some(3));
        let registry =
            ProjectRegistry::with_translator_template(4, template_source.configuration_template());
        let project_id = ProjectId::new("fixture").unwrap();
        registry
            .add(ProjectIdentity::new(
                project_id.clone(),
                CanonicalRoot::new(root.path()).unwrap(),
            ))
            .await
            .unwrap();
        registry.activate(&project_id).await.unwrap();

        let server =
            McplsServer::new_with_registry(Arc::new(ResourceSubscriptions::new()), registry);
        let rename = server
            .rename_preview(Parameters(RenamePreviewParams {
                project_id: project_id.as_str().to_string(),
                file_path: source.display().to_string(),
                line: 1,
                character: 1,
                new_name: "new_name".to_string(),
                position_encoding: Some("utf-8".to_string()),
            }))
            .await
            .unwrap();
        let rename: serde_json::Value = serde_json::from_str(&rename).unwrap();
        assert_eq!(rename["affected_files"].as_array().unwrap().len(), 2);
        assert_eq!(fs::read_to_string(&counter).unwrap(), "1");

        let applied = server
            .rename_apply(Parameters(WorkspaceEditApplyParams {
                project_id: project_id.as_str().to_string(),
                plan_id: rename["plan_id"].as_str().unwrap().to_string(),
                wait_timeout_ms: None,
            }))
            .await
            .unwrap();
        let applied: serde_json::Value = serde_json::from_str(&applied).unwrap();
        assert_eq!(applied["committed_files"].as_array().unwrap().len(), 2);
        assert_eq!(fs::read_to_string(&source).unwrap(), "new_name\n");
        assert_eq!(fs::read_to_string(&sibling).unwrap(), "new_name();\n");
        assert_eq!(fs::read_to_string(&counter).unwrap(), "1");

        let format = server
            .format_preview(Parameters(FormatPreviewParams {
                project_id: project_id.as_str().to_string(),
                file_path: source.display().to_string(),
                tab_size: 4,
                insert_spaces: true,
                position_encoding: Some("utf-8".to_string()),
                range: None,
            }))
            .await
            .unwrap();
        let format: serde_json::Value = serde_json::from_str(&format).unwrap();
        assert_eq!(fs::read_to_string(&counter).unwrap(), "2");
        let formatted = server
            .format_apply(Parameters(WorkspaceEditApplyParams {
                project_id: project_id.as_str().to_string(),
                plan_id: format["plan_id"].as_str().unwrap().to_string(),
                wait_timeout_ms: None,
            }))
            .await
            .unwrap();
        let formatted: serde_json::Value = serde_json::from_str(&formatted).unwrap();
        assert_eq!(fs::read_to_string(&source).unwrap(), "formatted\n");
        assert_eq!(fs::read_to_string(&counter).unwrap(), "2");

        let parsed: serde_json::Value = serde_json::from_str(
            &server
                .structural_replace_preview(Parameters(StructuralReplacePreviewParams {
                    project_id: project_id.as_str().to_string(),
                    file_path: source.display().to_string(),
                    dialect: "rust_analyzer_ssr".to_string(),
                    query: "formatted ==>> structural".to_string(),
                    replacement: None,
                    language_id: None,
                    parse_only: true,
                    position_encoding: Some("utf-8".to_string()),
                }))
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(parsed["parse_only"], true);
        assert_eq!(parsed["match_count"], 0);
        assert!(parsed.get("plan_id").is_none());
        assert_eq!(fs::read_to_string(&counter).unwrap(), "3");

        let structural: serde_json::Value = serde_json::from_str(
            &server
                .structural_replace_preview(Parameters(StructuralReplacePreviewParams {
                    project_id: project_id.as_str().to_string(),
                    file_path: source.display().to_string(),
                    dialect: "rust_analyzer_ssr".to_string(),
                    query: "formatted ==>> structural".to_string(),
                    replacement: None,
                    language_id: None,
                    parse_only: false,
                    position_encoding: Some("utf-8".to_string()),
                }))
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(structural["engine"], "rust_analyzer");
        assert_eq!(structural["match_count"], 1);
        assert_eq!(structural["producer"], "rust_analyzer");
        assert_eq!(structural["verification"], "semantic_verified");
        assert_eq!(fs::read_to_string(&counter).unwrap(), "4");
        server
            .workspace_edit_apply(Parameters(WorkspaceEditApplyParams {
                project_id: project_id.as_str().to_string(),
                plan_id: structural["plan_id"].as_str().unwrap().to_string(),
                wait_timeout_ms: None,
            }))
            .await
            .unwrap();
        assert_eq!(fs::read_to_string(&source).unwrap(), "structural\n");
        assert_eq!(fs::read_to_string(&counter).unwrap(), "4");

        let same_path = server
            .path_rename_preview(Parameters(PathRenamePreviewParams {
                project_id: project_id.as_str().to_string(),
                old_path: source.display().to_string(),
                new_path: source.display().to_string(),
                position_encoding: Some("utf-8".to_string()),
            }))
            .await
            .unwrap_err()
            .to_string();
        assert!(same_path.contains("same path"), "{same_path}");
        let outside = server
            .path_rename_preview(Parameters(PathRenamePreviewParams {
                project_id: project_id.as_str().to_string(),
                old_path: source.display().to_string(),
                new_path: root.path().join("../outside.rs").display().to_string(),
                position_encoding: Some("utf-8".to_string()),
            }))
            .await
            .unwrap_err()
            .to_string();
        assert!(outside.contains("escapes workspace"), "{outside}");
        let directory = root.path().join("directory");
        fs::create_dir(&directory).unwrap();
        let contained = server
            .path_rename_preview(Parameters(PathRenamePreviewParams {
                project_id: project_id.as_str().to_string(),
                old_path: directory.display().to_string(),
                new_path: directory.join("nested").display().to_string(),
                position_encoding: Some("utf-8".to_string()),
            }))
            .await
            .unwrap_err()
            .to_string();
        assert!(contained.contains("into itself"), "{contained}");
        assert_eq!(fs::read_to_string(&counter).unwrap(), "4");

        let renamed = root.path().join("renamed.rs");
        let stale_preview: serde_json::Value = serde_json::from_str(
            &server
                .path_rename_preview(Parameters(PathRenamePreviewParams {
                    project_id: project_id.as_str().to_string(),
                    old_path: source.display().to_string(),
                    new_path: renamed.display().to_string(),
                    position_encoding: Some("utf-8".to_string()),
                }))
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(stale_preview["semantic_providers"][0], "rust");
        assert_eq!(stale_preview["semantic_edit_count"], 1);
        assert_eq!(stale_preview["verification"], "semantic_verified");
        assert_eq!(
            stale_preview["operations"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|operation| operation
                    .as_str()
                    .is_some_and(|value| value.starts_with("rename ")))
                .count(),
            1
        );
        fs::write(&renamed, "occupied\n").unwrap();
        let stale_result = server
            .workspace_edit_apply(Parameters(WorkspaceEditApplyParams {
                project_id: project_id.as_str().to_string(),
                plan_id: stale_preview["plan_id"].as_str().unwrap().to_string(),
                wait_timeout_ms: None,
            }))
            .await
            .unwrap();
        let stale_result: serde_json::Value = serde_json::from_str(&stale_result).unwrap();
        assert_eq!(stale_result["status"], "conflict");
        assert_eq!(stale_result["retry"]["action"], "preview_again");
        assert_eq!(fs::read_to_string(&source).unwrap(), "structural\n");
        assert_eq!(fs::read_to_string(&sibling).unwrap(), "new_name();\n");
        fs::remove_file(&renamed).unwrap();

        let path_preview: serde_json::Value = serde_json::from_str(
            &server
                .path_rename_preview(Parameters(PathRenamePreviewParams {
                    project_id: project_id.as_str().to_string(),
                    old_path: source.display().to_string(),
                    new_path: renamed.display().to_string(),
                    position_encoding: Some("utf-8".to_string()),
                }))
                .await
                .unwrap(),
        )
        .unwrap();
        let path_applied: serde_json::Value = serde_json::from_str(
            &server
                .workspace_edit_apply(Parameters(WorkspaceEditApplyParams {
                    project_id: project_id.as_str().to_string(),
                    plan_id: path_preview["plan_id"].as_str().unwrap().to_string(),
                    wait_timeout_ms: None,
                }))
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            path_applied["semantic_state"], "synchronized",
            "{path_applied}"
        );
        assert_eq!(
            path_applied["provider_synchronization"][0]["synchronized"],
            true
        );
        assert!(!source.exists());
        assert_eq!(fs::read_to_string(&renamed).unwrap(), "structural\n");
        assert_eq!(fs::read_to_string(&sibling).unwrap(), "path_ref();\n");
        assert_eq!(fs::read_to_string(&counter).unwrap(), "6");

        let ranged: serde_json::Value = serde_json::from_str(
            &server
                .format_preview(Parameters(FormatPreviewParams {
                    project_id: project_id.as_str().to_string(),
                    file_path: renamed.display().to_string(),
                    tab_size: 4,
                    insert_spaces: true,
                    position_encoding: Some("utf-8".to_string()),
                    range: Some(FormatRange {
                        start_line: 1,
                        start_character: 1,
                        end_line: 2,
                        end_character: 1,
                    }),
                }))
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(ranged["supported"], true);
        assert_eq!(ranged["changed"], true);

        let fresh_range: serde_json::Value = serde_json::from_str(
            &server
                .range_format_preview(Parameters(RangeFormatPreviewParams {
                    project_id: project_id.as_str().to_string(),
                    file_path: renamed.display().to_string(),
                    start_line: 1,
                    start_character: 1,
                    end_line: 2,
                    end_character: 1,
                    tab_size: 4,
                    insert_spaces: true,
                    position_encoding: Some("utf-8".to_string()),
                }))
                .await
                .unwrap(),
        )
        .unwrap();
        fs::write(&renamed, "disk diverged from the open document\n").unwrap();
        let fresh_range_conflict = server
            .workspace_edit_apply(Parameters(WorkspaceEditApplyParams {
                project_id: project_id.as_str().to_string(),
                plan_id: fresh_range["plan_id"].as_str().unwrap().to_string(),
                wait_timeout_ms: None,
            }))
            .await
            .unwrap();
        let fresh_range_conflict: serde_json::Value =
            serde_json::from_str(&fresh_range_conflict).unwrap();
        assert_eq!(fresh_range_conflict["status"], "conflict");
        assert_eq!(
            fs::read_to_string(&renamed).unwrap(),
            "disk diverged from the open document\n"
        );
        let stale_range = server
            .workspace_edit_apply(Parameters(WorkspaceEditApplyParams {
                project_id: project_id.as_str().to_string(),
                plan_id: ranged["plan_id"].as_str().unwrap().to_string(),
                wait_timeout_ms: None,
            }))
            .await
            .unwrap();
        let stale_range: serde_json::Value = serde_json::from_str(&stale_range).unwrap();
        assert_eq!(stale_range["status"], "conflict");

        fs::write(&renamed, "structural\n").unwrap();
        let retry_range: serde_json::Value = serde_json::from_str(
            &server
                .range_format_preview(Parameters(RangeFormatPreviewParams {
                    project_id: project_id.as_str().to_string(),
                    file_path: renamed.display().to_string(),
                    start_line: 1,
                    start_character: 1,
                    end_line: 2,
                    end_character: 1,
                    tab_size: 4,
                    insert_spaces: true,
                    position_encoding: Some("utf-8".to_string()),
                }))
                .await
                .unwrap(),
        )
        .unwrap();
        server
            .workspace_edit_apply(Parameters(WorkspaceEditApplyParams {
                project_id: project_id.as_str().to_string(),
                plan_id: retry_range["plan_id"].as_str().unwrap().to_string(),
                wait_timeout_ms: None,
            }))
            .await
            .unwrap();
        assert_eq!(fs::read_to_string(&renamed).unwrap(), "ranged");

        let no_range: serde_json::Value = serde_json::from_str(
            &server
                .range_format_preview(Parameters(RangeFormatPreviewParams {
                    project_id: project_id.as_str().to_string(),
                    file_path: renamed.display().to_string(),
                    start_line: 98,
                    start_character: 1,
                    end_line: 98,
                    end_character: 1,
                    tab_size: 4,
                    insert_spaces: true,
                    position_encoding: Some("utf-8".to_string()),
                }))
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(no_range["supported"], true);
        assert_eq!(no_range["changed"], false);
        assert!(no_range.get("plan_id").is_none());

        let unicode = root.path().join("unicode.rs");
        fs::write(&unicode, "éx\n").unwrap();
        let unicode_range: serde_json::Value = serde_json::from_str(
            &server
                .range_format_preview(Parameters(RangeFormatPreviewParams {
                    project_id: project_id.as_str().to_string(),
                    file_path: unicode.display().to_string(),
                    start_line: 1,
                    start_character: 2,
                    end_line: 1,
                    end_character: 3,
                    tab_size: 4,
                    insert_spaces: true,
                    position_encoding: Some("utf-8".to_string()),
                }))
                .await
                .unwrap(),
        )
        .unwrap();
        server
            .workspace_edit_apply(Parameters(WorkspaceEditApplyParams {
                project_id: project_id.as_str().to_string(),
                plan_id: unicode_range["plan_id"].as_str().unwrap().to_string(),
                wait_timeout_ms: None,
            }))
            .await
            .unwrap();
        assert_eq!(fs::read_to_string(&unicode).unwrap(), "éranged\n");

        let semantic_position = || SemanticPositionParams {
            project_id: project_id.as_str().to_string(),
            file_path: renamed.display().to_string(),
            line: 1,
            character: 1,
            symbol_handle: None,
        };
        let declaration: serde_json::Value = serde_json::from_str(
            &server
                .get_declaration(Parameters(semantic_position()))
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(declaration["supported"], true);
        assert_eq!(declaration["provider"], "standard_lsp");
        assert_eq!(declaration["locations"][0]["range"]["start"]["line"], 2);

        let parent: serde_json::Value = serde_json::from_str(
            &server
                .get_parent_module(Parameters(semantic_position()))
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(parent["provider"], "rust_analyzer");
        assert_eq!(parent["locations"][0]["range"]["start"]["line"], 3);
        let children: serde_json::Value = serde_json::from_str(
            &server
                .get_child_modules(Parameters(semantic_position()))
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(children["locations"][0]["range"]["start"]["line"], 4);

        let expansion: serde_json::Value = serde_json::from_str(
            &server
                .expand_macro(Parameters(semantic_position()))
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(expansion["macro_expansion"]["name"], "fixture!");
        assert_eq!(
            expansion["macro_expansion"]["expansion"],
            "fn expanded() {}"
        );

        let selections: serde_json::Value = serde_json::from_str(
            &server
                .get_selection_ranges(Parameters(semantic_position()))
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(selections["provider"], "standard_lsp");
        assert_eq!(selections["selection_ranges"].as_array().unwrap().len(), 2);
        assert_eq!(selections["selection_ranges"][0]["start"]["character"], 3);
        assert_eq!(selections["selection_ranges"][1]["start"]["character"], 1);

        let runnables: serde_json::Value = serde_json::from_str(
            &server
                .discover_runnables(Parameters(semantic_position()))
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(runnables["runnables"][0]["label"], "test fixture");
        assert_eq!(runnables["runnables"][0]["args"]["cargoArgs"][0], "test");
        let related: serde_json::Value = serde_json::from_str(
            &server
                .discover_related_tests(Parameters(semantic_position()))
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(related["runnables"][0]["runnable"]["label"], "test related");

        let no_move: serde_json::Value = serde_json::from_str(
            &server
                .move_item_preview(Parameters(MoveItemPreviewParams {
                    project_id: project_id.as_str().to_string(),
                    file_path: renamed.display().to_string(),
                    start_line: 99,
                    start_character: 1,
                    end_line: 99,
                    end_character: 1,
                    direction: "down".to_string(),
                    position_encoding: Some("utf-8".to_string()),
                }))
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(no_move["supported"], true);
        assert_eq!(no_move["changed"], false);
        assert!(no_move.get("plan_id").is_none());

        let snippet = server
            .move_item_preview(Parameters(MoveItemPreviewParams {
                project_id: project_id.as_str().to_string(),
                file_path: renamed.display().to_string(),
                start_line: 1,
                start_character: 1,
                end_line: 1,
                end_character: 7,
                direction: "up".to_string(),
                position_encoding: Some("utf-8".to_string()),
            }))
            .await
            .unwrap_err()
            .to_string();
        assert!(snippet.contains("unresolved snippet"), "{snippet}");

        let moved: serde_json::Value = serde_json::from_str(
            &server
                .move_item_preview(Parameters(MoveItemPreviewParams {
                    project_id: project_id.as_str().to_string(),
                    file_path: renamed.display().to_string(),
                    start_line: 1,
                    start_character: 1,
                    end_line: 1,
                    end_character: 7,
                    direction: "down".to_string(),
                    position_encoding: Some("utf-8".to_string()),
                }))
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(moved["supported"], true);
        assert_eq!(moved["changed"], true);
        server
            .workspace_edit_apply(Parameters(WorkspaceEditApplyParams {
                project_id: project_id.as_str().to_string(),
                plan_id: moved["plan_id"].as_str().unwrap().to_string(),
                wait_timeout_ms: None,
            }))
            .await
            .unwrap();
        assert_eq!(fs::read_to_string(&renamed).unwrap(), "moved\n");

        let request_count = fs::read_to_string(&counter).unwrap();
        let retried = server
            .format_apply(Parameters(WorkspaceEditApplyParams {
                project_id: project_id.as_str().to_string(),
                plan_id: format["plan_id"].as_str().unwrap().to_string(),
                wait_timeout_ms: None,
            }))
            .await
            .unwrap();
        let retried: serde_json::Value = serde_json::from_str(&retried).unwrap();
        assert_eq!(retried, formatted);
        assert_eq!(fs::read_to_string(&counter).unwrap(), request_count);
    }

    #[cfg(unix)]
    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn local_edit_previews_report_unsupported_capabilities_without_plans() {
        use std::collections::HashMap;
        use std::fs;
        use std::os::unix::fs::PermissionsExt;

        let root = TempDir::new().unwrap();
        let source = root.path().join("source.rs");
        fs::write(&source, "fn first() {}\nfn second() {}\n").unwrap();
        let lsp = root.path().join("fake-edit-lsp.py");
        fs::write(
            &lsp,
            FAKE_EDIT_LSP
                .replace("if True:", "if False:")
                .replace("\"name\": \"rust-analyzer\"", "\"name\": \"generic-lsp\""),
        )
        .unwrap();
        let mut permissions = fs::metadata(&lsp).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&lsp, permissions).unwrap();

        let mut config = crate::config::LspServerConfig::rust_analyzer();
        config.command = lsp.display().to_string();
        config.heuristics = None;
        config.env = HashMap::from([(
            "MCPLS_COUNTER".to_string(),
            root.path().join("counter").display().to_string(),
        )]);
        let mut translator = Translator::new()
            .with_extensions(HashMap::from([("rs".to_string(), "rust".to_string())]));
        translator.set_lsp_configs(vec![config], Some(3));
        let registry =
            ProjectRegistry::with_translator_template(4, translator.configuration_template());
        let project_id = ProjectId::new("fixture").unwrap();
        registry
            .add(ProjectIdentity::new(
                project_id.clone(),
                CanonicalRoot::new(root.path()).unwrap(),
            ))
            .await
            .unwrap();
        registry.activate(&project_id).await.unwrap();
        let server =
            McplsServer::new_with_registry(Arc::new(ResourceSubscriptions::new()), registry);

        let range: serde_json::Value = serde_json::from_str(
            &server
                .format_preview(Parameters(FormatPreviewParams {
                    project_id: project_id.as_str().to_string(),
                    file_path: source.display().to_string(),
                    tab_size: 4,
                    insert_spaces: true,
                    position_encoding: None,
                    range: Some(FormatRange {
                        start_line: 1,
                        start_character: 1,
                        end_line: 1,
                        end_character: 3,
                    }),
                }))
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(range["supported"], false);
        assert_eq!(range["changed"], false);
        assert!(range.get("plan_id").is_none());

        let movement: serde_json::Value = serde_json::from_str(
            &server
                .move_item_preview(Parameters(MoveItemPreviewParams {
                    project_id: project_id.as_str().to_string(),
                    file_path: source.display().to_string(),
                    start_line: 1,
                    start_character: 1,
                    end_line: 1,
                    end_character: 3,
                    direction: "down".to_string(),
                    position_encoding: None,
                }))
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(movement["supported"], false);
        assert_eq!(movement["changed"], false);
        assert!(movement.get("plan_id").is_none());

        let semantic_position = || SemanticPositionParams {
            project_id: project_id.as_str().to_string(),
            file_path: source.display().to_string(),
            line: 1,
            character: 1,
            symbol_handle: None,
        };
        for response in [
            server
                .get_declaration(Parameters(semantic_position()))
                .await
                .unwrap(),
            server
                .get_selection_ranges(Parameters(semantic_position()))
                .await
                .unwrap(),
            server
                .get_parent_module(Parameters(semantic_position()))
                .await
                .unwrap(),
            server
                .expand_macro(Parameters(semantic_position()))
                .await
                .unwrap(),
            server
                .discover_runnables(Parameters(semantic_position()))
                .await
                .unwrap(),
            server
                .discover_related_tests(Parameters(semantic_position()))
                .await
                .unwrap(),
        ] {
            let response: serde_json::Value = serde_json::from_str(&response).unwrap();
            assert_eq!(response["supported"], false);
            assert_eq!(response["truncated"], false);
        }

        let outside = TempDir::new().unwrap();
        let outside_source = outside.path().join("outside.rs");
        fs::write(&outside_source, "fn outside() {}\n").unwrap();
        let isolated = server
            .get_declaration(Parameters(SemanticPositionParams {
                project_id: project_id.as_str().to_string(),
                file_path: outside_source.display().to_string(),
                line: 1,
                character: 1,
                symbol_handle: None,
            }))
            .await
            .unwrap_err()
            .to_string();
        assert!(isolated.contains("outside workspace"), "{isolated}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn provider_exit_during_resource_sync_keeps_the_committed_apply_result() {
        let (root, source, sibling, server) = fake_text_edit_fixture(true).await;
        let destination = root.path().join("renamed.rs");
        let project_id = ProjectId::new("fixture").unwrap();

        let preview: serde_json::Value = serde_json::from_str(
            &server
                .path_rename_preview(Parameters(PathRenamePreviewParams {
                    project_id: project_id.as_str().to_string(),
                    old_path: source.display().to_string(),
                    new_path: destination.display().to_string(),
                    position_encoding: Some("utf-8".to_string()),
                }))
                .await
                .unwrap(),
        )
        .unwrap();
        let applied: serde_json::Value = serde_json::from_str(
            &server
                .workspace_edit_apply(Parameters(WorkspaceEditApplyParams {
                    project_id: project_id.as_str().to_string(),
                    plan_id: preview["plan_id"].as_str().unwrap().to_string(),
                    wait_timeout_ms: None,
                }))
                .await
                .unwrap(),
        )
        .unwrap();

        assert_eq!(applied["semantic_state"], "degraded");
        assert_eq!(
            applied["provider_synchronization"][0]["synchronized"],
            false
        );
        let message = applied["provider_synchronization"][0]["message"]
            .as_str()
            .unwrap();
        assert!(message.contains("failed"), "{message}");
        assert!(!message.contains("no dynamic"), "{message}");
        assert!(!source.exists());
        assert_eq!(std::fs::read_to_string(destination).unwrap(), "old_name\n");
        assert_eq!(std::fs::read_to_string(sibling).unwrap(), "path_ref();\n");
    }

    #[tokio::test]
    async fn workspace_edit_preview_and_apply_support_file_rename() {
        let root = TempDir::new().unwrap();
        let old = root.path().join("old.rs");
        let renamed = root.path().join("renamed.rs");
        std::fs::write(&old, "content\n").unwrap();
        let registry = ProjectRegistry::new(2);
        registry
            .add(ProjectIdentity::new(
                ProjectId::new("project").unwrap(),
                CanonicalRoot::new(root.path()).unwrap(),
            ))
            .await
            .unwrap();
        let server =
            McplsServer::new_with_registry(Arc::new(ResourceSubscriptions::new()), registry);
        let result = server
            .workspace_edit_preview(Parameters(WorkspaceEditPreviewParams {
                project_id: "project".to_string(),
                workspace_edit: serde_json::json!({
                    "documentChanges": [{
                        "kind": "rename",
                        "oldUri": url::Url::from_file_path(&old).unwrap().to_string(),
                        "newUri": url::Url::from_file_path(&renamed).unwrap().to_string()
                    }]
                }),
                position_encoding: None,
            }))
            .await
            .unwrap();
        let result: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(result["safe_to_apply"], true);
        assert!(result["unsupported"].as_array().unwrap().is_empty());

        server
            .workspace_edit_apply(Parameters(WorkspaceEditApplyParams {
                project_id: "project".to_string(),
                plan_id: result["plan_id"].as_str().unwrap().to_string(),
                wait_timeout_ms: None,
            }))
            .await
            .unwrap();
        assert!(!old.exists());
        assert_eq!(std::fs::read_to_string(renamed).unwrap(), "content\n");
    }

    #[tokio::test]
    async fn path_rename_without_provider_is_explicitly_unverified() {
        let root = TempDir::new().unwrap();
        let old = root.path().join("old.rs");
        let renamed = root.path().join("renamed.rs");
        std::fs::write(&old, "content\n").unwrap();
        let registry = ProjectRegistry::new(2);
        registry
            .add(ProjectIdentity::new(
                ProjectId::new("project").unwrap(),
                CanonicalRoot::new(root.path()).unwrap(),
            ))
            .await
            .unwrap();
        let server =
            McplsServer::new_with_registry(Arc::new(ResourceSubscriptions::new()), registry);

        let preview: serde_json::Value = serde_json::from_str(
            &server
                .path_rename_preview(Parameters(PathRenamePreviewParams {
                    project_id: "project".to_string(),
                    old_path: old.display().to_string(),
                    new_path: renamed.display().to_string(),
                    position_encoding: None,
                }))
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(preview["semantic_provider_available"], false);
        assert_eq!(preview["semantic_providers"], serde_json::json!([]));
        assert_eq!(preview["semantic_edit_count"], 0);
        assert_eq!(preview["verification"], "structural_unverified");
        assert_eq!(preview["producer"], serde_json::Value::Null);
        assert_eq!(preview["operations"].as_array().unwrap().len(), 1);

        server
            .workspace_edit_apply(Parameters(WorkspaceEditApplyParams {
                project_id: "project".to_string(),
                plan_id: preview["plan_id"].as_str().unwrap().to_string(),
                wait_timeout_ms: None,
            }))
            .await
            .unwrap();
        assert!(!old.exists());
        assert_eq!(std::fs::read_to_string(renamed).unwrap(), "content\n");
    }

    #[tokio::test]
    async fn test_project_activate_uses_actor_runtime() {
        let root = TempDir::new().unwrap();
        std::fs::write(
            root.path().join("Cargo.toml"),
            "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        )
        .unwrap();
        let mut translator = Translator::new();
        let mut config = crate::config::LspServerConfig::rust_analyzer();
        config.command = "/definitely/missing/custom-rust-lsp".to_string();
        translator.set_lsp_configs(vec![config], Some(1));
        let template = translator.configuration_template();
        let subscriptions = Arc::new(ResourceSubscriptions::new());
        let registry = ProjectRegistry::with_translator_template(2, template);
        let server = McplsServer::new_with_registry(subscriptions, registry);

        server
            .project_add(Parameters(ProjectAddParams {
                project_id: "fixture".to_string(),
                root: root.path().display().to_string(),
                config: None,
            }))
            .await
            .unwrap();

        let result = server
            .project_activate(Parameters(ProjectIdParams {
                project_id: "fixture".to_string(),
            }))
            .await;

        assert!(result.is_err());
        let state = server
            .project_status(Parameters(ProjectIdParams {
                project_id: "fixture".to_string(),
            }))
            .await
            .unwrap();
        assert!(state.contains("Failed"));
        assert!(state.contains("rust"));
    }

    #[tokio::test]
    async fn project_configure_cargo_features_returns_effective_profile() {
        let root = TempDir::new().unwrap();
        let registry = ProjectRegistry::new(2);
        let server =
            McplsServer::new_with_registry(Arc::new(ResourceSubscriptions::new()), registry);
        server
            .project_add(project_add_params("fixture", root.path()))
            .await
            .unwrap();

        let result = server
            .project_configure_cargo_features(Parameters(ProjectCargoFeaturesParams {
                project_id: "fixture".to_owned(),
                features: vec!["zeta".to_owned(), "alpha".to_owned(), "alpha".to_owned()],
                all_features: false,
                no_default_features: true,
            }))
            .await
            .unwrap();
        let result: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(
            result["cargo_features"],
            serde_json::json!({
                "features": ["alpha", "zeta"],
                "all_features": false,
                "no_default_features": true
            })
        );
    }

    #[tokio::test]
    async fn project_activate_without_applicable_lsp_reaches_degraded_fallback() {
        let root = TempDir::new().unwrap();
        std::fs::write(root.path().join("main.rs"), "fn fallback_symbol() {}\n").unwrap();
        let mut translator = Translator::new();
        translator.set_lsp_configs(
            vec![crate::config::LspServerConfig::rust_analyzer()],
            Some(1),
        );
        let registry =
            ProjectRegistry::with_translator_template(2, translator.configuration_template());
        let server =
            McplsServer::new_with_registry(Arc::new(ResourceSubscriptions::new()), registry);

        server
            .project_add(Parameters(ProjectAddParams {
                project_id: "fallback-only".to_string(),
                root: root.path().display().to_string(),
                config: None,
            }))
            .await
            .unwrap();
        let activated = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            server.project_activate(Parameters(ProjectIdParams {
                project_id: "fallback-only".to_string(),
            })),
        )
        .await
        .unwrap()
        .unwrap();
        let activated: serde_json::Value = serde_json::from_str(&activated).unwrap();
        let symbols = server
            .workspace_symbol_search(Parameters(WorkspaceSymbolParams {
                project_id: "fallback-only".to_string(),
                query: Some("fallback_symbol".to_string()),
                queries: Vec::new(),
                kind_filter: None,
                match_mode: crate::bridge::WorkspaceSymbolMatchMode::default(),
                scope: crate::bridge::WorkspaceSymbolScope::default(),
                limit: 20,
                max_bytes: 16 * 1024,
                page_token: None,
                include_generated: false,
            }))
            .await
            .unwrap();
        let symbols: serde_json::Value = serde_json::from_str(&symbols).unwrap();
        let handle: SymbolHandle =
            serde_json::from_value(symbols["symbols"][0]["location"]["symbol_handle"].clone())
                .unwrap();
        let handle_error = server
            .for_session()
            .get_hover(Parameters(HoverParams {
                file_path: String::new(),
                line: 0,
                character: 0,
                project_id: Some("fallback-only".to_owned()),
                symbol_handle: Some(handle),
            }))
            .await
            .unwrap_err()
            .to_string();
        let repeated = server
            .project_activate(Parameters(ProjectIdParams {
                project_id: "fallback-only".to_string(),
            }))
            .await
            .unwrap();
        let repeated: serde_json::Value = serde_json::from_str(&repeated).unwrap();
        let hover_error = server
            .get_hover(Parameters(HoverParams {
                file_path: root.path().join("main.rs").display().to_string(),
                line: 1,
                character: 1,
                project_id: None,
                symbol_handle: None,
            }))
            .await
            .unwrap_err()
            .to_string();

        assert_eq!(activated["status"], "Degraded");
        assert_eq!(activated["active_language_servers"], serde_json::json!([]));
        assert_eq!(symbols["symbols"][0]["name"], "fallback_symbol");
        assert!(
            handle_error.contains("no LSP server configured"),
            "{handle_error}"
        );
        assert_eq!(repeated["generation"], activated["generation"]);
        assert!(
            hover_error.contains("no LSP server configured for language:"),
            "{hover_error}"
        );
    }

    #[tokio::test]
    async fn project_activate_without_optional_executable_keeps_ast_fallback() {
        let root = TempDir::new().unwrap();
        std::fs::write(
            root.path().join("pyproject.toml"),
            "[project]\nname=\"fixture\"\n",
        )
        .unwrap();
        std::fs::write(
            root.path().join("main.py"),
            "def missing_server_fallback():\n    return 42\n",
        )
        .unwrap();

        let mut config = crate::config::LspServerConfig::pyright();
        config.command = "/definitely-missing/pyright-langserver".to_string();
        let mut translator = Translator::new();
        translator.set_lsp_configs(vec![config], Some(2));
        let registry =
            ProjectRegistry::with_translator_template(2, translator.configuration_template());
        let server =
            McplsServer::new_with_registry(Arc::new(ResourceSubscriptions::new()), registry);

        server
            .project_add(Parameters(ProjectAddParams {
                project_id: "missing-optional".to_string(),
                root: root.path().display().to_string(),
                config: None,
            }))
            .await
            .unwrap();
        let activated = server
            .project_activate(Parameters(ProjectIdParams {
                project_id: "missing-optional".to_string(),
            }))
            .await
            .unwrap();
        let activated: serde_json::Value = serde_json::from_str(&activated).unwrap();
        assert_eq!(activated["status"], "Degraded");
        assert_eq!(activated["active_language_servers"], serde_json::json!([]));

        let symbols = server
            .workspace_symbol_search(Parameters(WorkspaceSymbolParams {
                project_id: "missing-optional".to_string(),
                query: Some("missing_server_fallback".to_string()),
                queries: Vec::new(),
                kind_filter: None,
                match_mode: crate::bridge::WorkspaceSymbolMatchMode::default(),
                scope: crate::bridge::WorkspaceSymbolScope::default(),
                limit: 20,
                max_bytes: 16 * 1024,
                page_token: None,
                include_generated: false,
            }))
            .await
            .unwrap();
        let symbols: serde_json::Value = serde_json::from_str(&symbols).unwrap();
        assert_eq!(symbols["symbols"][0]["name"], "missing_server_fallback");
    }

    #[tokio::test]
    async fn test_server_can_share_an_injected_registry() {
        let subscriptions = Arc::new(ResourceSubscriptions::new());
        let registry = crate::project::ProjectRegistry::new(2);
        let server = McplsServer::new_with_registry(subscriptions, registry.clone());
        let root = TempDir::new().unwrap();
        registry
            .add(crate::project::ProjectIdentity::new(
                crate::project::ProjectId::new("shared").unwrap(),
                crate::project::CanonicalRoot::new(root.path()).unwrap(),
            ))
            .await
            .unwrap();

        let listed = server
            .project_list(Parameters(ProjectListParams::default()))
            .await
            .unwrap();
        assert!(listed.contains("shared"));
    }

    #[tokio::test]
    async fn test_hover_tool_with_params() {
        let server = create_test_server();
        let params = Parameters(HoverParams {
            file_path: "/nonexistent/file.rs".to_string(),
            line: 1,
            character: 1,
            project_id: None,
            symbol_handle: None,
        });

        // This should return an error (no LSP server configured)
        let result = server.get_hover(params).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn semantic_tools_reject_unregistered_paths_without_global_fallback() {
        let server = create_test_server();
        let root = TempDir::new().unwrap();
        let file = root.path().join("unregistered.rs");
        std::fs::write(&file, "fn main() {}\n").unwrap();
        let error = server
            .get_hover(Parameters(HoverParams {
                file_path: file.display().to_string(),
                line: 1,
                character: 1,
                project_id: None,
                symbol_handle: None,
            }))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("path is not registered"), "{error}");
    }

    #[tokio::test]
    async fn test_hover_routes_registered_paths_to_project_actor() {
        let project_root = TempDir::new().unwrap();
        let file_path = project_root.path().join("src.rs");
        std::fs::write(&file_path, "fn main() {}\n").unwrap();

        let subscriptions = Arc::new(ResourceSubscriptions::new());
        let registry = ProjectRegistry::new(2);
        registry
            .add(ProjectIdentity::new(
                ProjectId::new("project").unwrap(),
                CanonicalRoot::new(project_root.path()).unwrap(),
            ))
            .await
            .unwrap();
        let server = McplsServer::new_with_registry(subscriptions, registry);

        let result = server
            .get_hover(Parameters(HoverParams {
                file_path: file_path.display().to_string(),
                line: 0,
                character: 0,
                project_id: None,
                symbol_handle: None,
            }))
            .await;

        let error = result.unwrap_err().to_string();
        assert!(!error.contains("outside workspace"), "{error}");
    }

    #[tokio::test]
    async fn test_definition_routes_registered_paths_to_project_actor() {
        let project_root = TempDir::new().unwrap();
        let file_path = project_root.path().join("src.rs");
        std::fs::write(&file_path, "fn main() {}\n").unwrap();

        let subscriptions = Arc::new(ResourceSubscriptions::new());
        let registry = ProjectRegistry::new(2);
        registry
            .add(ProjectIdentity::new(
                ProjectId::new("project").unwrap(),
                CanonicalRoot::new(project_root.path()).unwrap(),
            ))
            .await
            .unwrap();
        let server = McplsServer::new_with_registry(subscriptions, registry);

        let result = server
            .get_definition(Parameters(DefinitionParams {
                file_path: file_path.display().to_string(),
                line: 0,
                character: 0,
                project_id: None,
                symbol_handle: None,
            }))
            .await;

        let error = result.unwrap_err().to_string();
        assert!(!error.contains("outside workspace"), "{error}");
    }

    #[tokio::test]
    async fn test_references_routes_registered_paths_to_project_actor() {
        let project_root = TempDir::new().unwrap();
        let file_path = project_root.path().join("src.rs");
        std::fs::write(&file_path, "fn main() {}\n").unwrap();

        let subscriptions = Arc::new(ResourceSubscriptions::new());
        let registry = ProjectRegistry::new(2);
        registry
            .add(ProjectIdentity::new(
                ProjectId::new("project").unwrap(),
                CanonicalRoot::new(project_root.path()).unwrap(),
            ))
            .await
            .unwrap();
        let server = McplsServer::new_with_registry(subscriptions, registry);

        let result = server
            .get_references(Parameters(ReferencesParams {
                file_path: file_path.display().to_string(),
                line: 0,
                character: 0,
                project_id: None,
                symbol_handle: None,
                include_declaration: false,
                limits: crate::bridge::SemanticResultLimits::default(),
                page_token: None,
            }))
            .await;

        let error = result.unwrap_err().to_string();
        assert!(!error.contains("outside workspace"), "{error}");
    }

    #[tokio::test]
    async fn test_diagnostics_routes_registered_paths_to_project_actor() {
        let project_root = TempDir::new().unwrap();
        let file_path = project_root.path().join("src.rs");
        std::fs::write(&file_path, "fn main() {}\n").unwrap();

        let subscriptions = Arc::new(ResourceSubscriptions::new());
        let registry = ProjectRegistry::new(2);
        registry
            .add(ProjectIdentity::new(
                ProjectId::new("project").unwrap(),
                CanonicalRoot::new(project_root.path()).unwrap(),
            ))
            .await
            .unwrap();
        let server = McplsServer::new_with_registry(subscriptions, registry);

        let result = server
            .get_diagnostics(Parameters(DiagnosticsParams {
                file_path: file_path.display().to_string(),
                mode: DiagnosticsMode::CacheOnly,
                fresh: None,
                options: DiagnosticOptions::default(),
            }))
            .await;

        let response = result.unwrap();
        let response: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(response["diagnostics"], serde_json::json!([]));
        assert_eq!(response["cache"]["hit"], false);
    }

    #[tokio::test]
    async fn test_rename_routes_registered_paths_to_project_actor() {
        let project_root = TempDir::new().unwrap();
        let file_path = project_root.path().join("src.rs");
        std::fs::write(&file_path, "fn main() {}\n").unwrap();

        let subscriptions = Arc::new(ResourceSubscriptions::new());
        let registry = ProjectRegistry::new(2);
        registry
            .add(ProjectIdentity::new(
                ProjectId::new("project").unwrap(),
                CanonicalRoot::new(project_root.path()).unwrap(),
            ))
            .await
            .unwrap();
        let server = McplsServer::new_with_registry(subscriptions, registry);

        let result = server
            .rename_symbol(Parameters(RenameParams {
                file_path: file_path.display().to_string(),
                line: 0,
                character: 0,
                new_name: "renamed".to_string(),
            }))
            .await;

        let error = result.unwrap_err().to_string();
        assert!(!error.contains("outside workspace"), "{error}");
    }

    #[tokio::test]
    async fn test_completions_routes_registered_paths_to_project_actor() {
        let project_root = TempDir::new().unwrap();
        let file_path = project_root.path().join("src.rs");
        std::fs::write(&file_path, "fn main() {}\n").unwrap();

        let subscriptions = Arc::new(ResourceSubscriptions::new());
        let registry = ProjectRegistry::new(2);
        registry
            .add(ProjectIdentity::new(
                ProjectId::new("project").unwrap(),
                CanonicalRoot::new(project_root.path()).unwrap(),
            ))
            .await
            .unwrap();
        let server = McplsServer::new_with_registry(subscriptions, registry);

        let result = server
            .get_completions(Parameters(CompletionsParams {
                file_path: file_path.display().to_string(),
                line: 0,
                character: 0,
                trigger: None,
            }))
            .await;

        let error = result.unwrap_err().to_string();
        assert!(!error.contains("outside workspace"), "{error}");
    }

    #[tokio::test]
    async fn test_document_symbols_routes_registered_paths_to_project_actor() {
        let project_root = TempDir::new().unwrap();
        let file_path = project_root.path().join("src.rs");
        std::fs::write(&file_path, "fn main() {}\n").unwrap();

        let subscriptions = Arc::new(ResourceSubscriptions::new());
        let registry = ProjectRegistry::new(2);
        registry
            .add(ProjectIdentity::new(
                ProjectId::new("project").unwrap(),
                CanonicalRoot::new(project_root.path()).unwrap(),
            ))
            .await
            .unwrap();
        let server = McplsServer::new_with_registry(subscriptions, registry);

        let result = server
            .get_document_symbols(Parameters(DocumentSymbolsParams {
                file_path: file_path.display().to_string(),
                options: DocumentSymbolOptions::default(),
                max_bytes: 16 * 1024,
                page_token: None,
            }))
            .await;

        let error = result.unwrap_err().to_string();
        assert!(!error.contains("outside workspace"), "{error}");
    }

    #[tokio::test]
    async fn test_format_routes_registered_paths_to_project_actor() {
        let project_root = TempDir::new().unwrap();
        let file_path = project_root.path().join("src.rs");
        std::fs::write(&file_path, "fn main() {}\n").unwrap();

        let subscriptions = Arc::new(ResourceSubscriptions::new());
        let registry = ProjectRegistry::new(2);
        registry
            .add(ProjectIdentity::new(
                ProjectId::new("project").unwrap(),
                CanonicalRoot::new(project_root.path()).unwrap(),
            ))
            .await
            .unwrap();
        let server = McplsServer::new_with_registry(subscriptions, registry);

        let result = server
            .format_document(Parameters(FormatDocumentParams {
                file_path: file_path.display().to_string(),
                tab_size: 4,
                insert_spaces: true,
            }))
            .await;

        let error = result.unwrap_err().to_string();
        assert!(!error.contains("outside workspace"), "{error}");
    }

    #[tokio::test]
    async fn test_code_actions_routes_registered_paths_to_project_actor() {
        let project_root = TempDir::new().unwrap();
        let file_path = project_root.path().join("src.rs");
        std::fs::write(&file_path, "fn main() {}\n").unwrap();

        let subscriptions = Arc::new(ResourceSubscriptions::new());
        let registry = ProjectRegistry::new(2);
        registry
            .add(ProjectIdentity::new(
                ProjectId::new("project").unwrap(),
                CanonicalRoot::new(project_root.path()).unwrap(),
            ))
            .await
            .unwrap();
        let server = McplsServer::new_with_registry(subscriptions, registry);

        let result = server
            .get_code_actions(Parameters(CodeActionsParams {
                file_path: file_path.display().to_string(),
                start_line: 1,
                start_character: 5,
                end_line: 1,
                end_character: 15,
                kind_filter: None,
            }))
            .await;

        let error = result.unwrap_err().to_string();
        assert!(!error.contains("outside workspace"), "{error}");
    }

    #[tokio::test]
    async fn test_call_hierarchy_routes_registered_paths_to_project_actor() {
        let project_root = TempDir::new().unwrap();
        let file_path = project_root.path().join("src.rs");
        std::fs::write(&file_path, "fn main() {}\n").unwrap();

        let subscriptions = Arc::new(ResourceSubscriptions::new());
        let registry = ProjectRegistry::new(2);
        registry
            .add(ProjectIdentity::new(
                ProjectId::new("project").unwrap(),
                CanonicalRoot::new(project_root.path()).unwrap(),
            ))
            .await
            .unwrap();
        let server = McplsServer::new_with_registry(subscriptions, registry);

        let result = server
            .prepare_call_hierarchy(Parameters(CallHierarchyPrepareParams {
                file_path: file_path.display().to_string(),
                line: 1,
                character: 5,
                project_id: None,
                page_token: None,
                symbol_handle: None,
            }))
            .await;

        let error = result.unwrap_err().to_string();
        assert!(!error.contains("outside workspace"), "{error}");
    }

    #[tokio::test]
    async fn test_signature_help_routes_registered_paths_to_project_actor() {
        let project_root = TempDir::new().unwrap();
        let file_path = project_root.path().join("src.rs");
        std::fs::write(&file_path, "fn main() {}\n").unwrap();

        let subscriptions = Arc::new(ResourceSubscriptions::new());
        let registry = ProjectRegistry::new(2);
        registry
            .add(ProjectIdentity::new(
                ProjectId::new("project").unwrap(),
                CanonicalRoot::new(project_root.path()).unwrap(),
            ))
            .await
            .unwrap();
        let server = McplsServer::new_with_registry(subscriptions, registry);

        let result = server
            .get_signature_help(Parameters(SignatureHelpParams {
                file_path: file_path.display().to_string(),
                line: 1,
                character: 5,
            }))
            .await;

        let error = result.unwrap_err().to_string();
        assert!(!error.contains("outside workspace"), "{error}");
    }

    #[tokio::test]
    async fn test_inlay_hints_routes_registered_paths_to_project_actor() {
        let project_root = TempDir::new().unwrap();
        let file_path = project_root.path().join("src.rs");
        std::fs::write(&file_path, "fn main() {}\n").unwrap();

        let subscriptions = Arc::new(ResourceSubscriptions::new());
        let registry = ProjectRegistry::new(2);
        registry
            .add(ProjectIdentity::new(
                ProjectId::new("project").unwrap(),
                CanonicalRoot::new(project_root.path()).unwrap(),
            ))
            .await
            .unwrap();
        let server = McplsServer::new_with_registry(subscriptions, registry);

        let result = server
            .get_inlay_hints(Parameters(InlayHintsParams {
                file_path: file_path.display().to_string(),
                start_line: 1,
                start_character: 5,
                end_line: 1,
                end_character: 15,
            }))
            .await;

        let error = result.unwrap_err().to_string();
        assert!(!error.contains("outside workspace"), "{error}");
    }

    #[tokio::test]
    async fn test_implementation_routes_registered_paths_to_project_actor() {
        let project_root = TempDir::new().unwrap();
        let file_path = project_root.path().join("src.rs");
        std::fs::write(&file_path, "fn main() {}\n").unwrap();
        let subscriptions = Arc::new(ResourceSubscriptions::new());
        let registry = ProjectRegistry::new(2);
        registry
            .add(ProjectIdentity::new(
                ProjectId::new("project").unwrap(),
                CanonicalRoot::new(project_root.path()).unwrap(),
            ))
            .await
            .unwrap();
        let server = McplsServer::new_with_registry(subscriptions, registry);

        let result = server
            .go_to_implementation(Parameters(GoToImplementationParams {
                file_path: file_path.display().to_string(),
                line: 1,
                character: 5,
                project_id: None,
                symbol_handle: None,
            }))
            .await;

        let error = result.unwrap_err().to_string();
        assert!(!error.contains("outside workspace"), "{error}");
    }

    #[tokio::test]
    async fn test_type_definition_routes_registered_paths_to_project_actor() {
        let project_root = TempDir::new().unwrap();
        let file_path = project_root.path().join("src.rs");
        std::fs::write(&file_path, "fn main() {}\n").unwrap();
        let subscriptions = Arc::new(ResourceSubscriptions::new());
        let registry = ProjectRegistry::new(2);
        registry
            .add(ProjectIdentity::new(
                ProjectId::new("project").unwrap(),
                CanonicalRoot::new(project_root.path()).unwrap(),
            ))
            .await
            .unwrap();
        let server = McplsServer::new_with_registry(subscriptions, registry);

        let result = server
            .go_to_type_definition(Parameters(GoToTypeDefinitionParams {
                file_path: file_path.display().to_string(),
                line: 1,
                character: 5,
                project_id: None,
                symbol_handle: None,
            }))
            .await;

        let error = result.unwrap_err().to_string();
        assert!(!error.contains("outside workspace"), "{error}");
    }

    #[tokio::test]
    async fn cache_only_diagnostics_never_requires_provider_analysis() {
        let project_root = TempDir::new().unwrap();
        let file_path = project_root.path().join("src.rs");
        std::fs::write(&file_path, "fn main() {}\n").unwrap();
        let subscriptions = Arc::new(ResourceSubscriptions::new());
        let registry = ProjectRegistry::new(2);
        registry
            .add(ProjectIdentity::new(
                ProjectId::new("project").unwrap(),
                CanonicalRoot::new(project_root.path()).unwrap(),
            ))
            .await
            .unwrap();
        let server = McplsServer::new_with_registry(subscriptions, registry);

        let result = server
            .get_diagnostics(Parameters(DiagnosticsParams {
                file_path: file_path.display().to_string(),
                mode: DiagnosticsMode::CacheOnly,
                fresh: None,
                options: DiagnosticOptions::default(),
            }))
            .await;

        let response = result.unwrap();
        let response: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(response["diagnostics"], serde_json::json!([]));
        assert_eq!(response["total_diagnostics"], 0);
    }

    #[tokio::test]
    async fn test_definition_tool_with_params() {
        let server = create_test_server();
        let params = Parameters(DefinitionParams {
            file_path: "/test/file.rs".to_string(),
            line: 10,
            character: 5,
            project_id: None,
            symbol_handle: None,
        });

        let result = server.get_definition(params).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_references_tool_with_params() {
        let server = create_test_server();
        let params = Parameters(ReferencesParams {
            file_path: "/test/file.rs".to_string(),
            line: 10,
            character: 5,
            project_id: None,
            symbol_handle: None,
            include_declaration: false,
            limits: crate::bridge::SemanticResultLimits::default(),
            page_token: None,
        });

        let result = server.get_references(params).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_diagnostics_tool_with_params() {
        let server = create_test_server();
        let params = Parameters(DiagnosticsParams {
            file_path: "/test/file.rs".to_string(),
            mode: DiagnosticsMode::CacheOnly,
            fresh: None,
            options: DiagnosticOptions::default(),
        });

        let result = server.get_diagnostics(params).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_rename_tool_with_params() {
        let server = create_test_server();
        let params = Parameters(RenameParams {
            file_path: "/test/file.rs".to_string(),
            line: 10,
            character: 5,
            new_name: "new_name".to_string(),
        });

        let result = server.rename_symbol(params).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_completions_tool_with_params() {
        let server = create_test_server();
        let params = Parameters(CompletionsParams {
            file_path: "/test/file.rs".to_string(),
            line: 10,
            character: 5,
            trigger: None,
        });

        let result = server.get_completions(params).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_document_symbols_tool_with_params() {
        let server = create_test_server();
        let params = Parameters(DocumentSymbolsParams {
            file_path: "/test/file.rs".to_string(),
            options: DocumentSymbolOptions::default(),
            max_bytes: 16 * 1024,
            page_token: None,
        });

        let result = server.get_document_symbols(params).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_format_document_tool_with_params() {
        let server = create_test_server();
        let params = Parameters(FormatDocumentParams {
            file_path: "/test/file.rs".to_string(),
            tab_size: 4,
            insert_spaces: true,
        });

        let result = server.format_document(params).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_workspace_symbol_search_tool_with_params() {
        let server = create_test_server();
        let params = Parameters(WorkspaceSymbolParams {
            project_id: "missing".to_string(),
            query: Some("User".to_string()),
            queries: Vec::new(),
            kind_filter: None,
            match_mode: crate::bridge::WorkspaceSymbolMatchMode::default(),
            scope: crate::bridge::WorkspaceSymbolScope::default(),
            limit: 100,
            max_bytes: 16 * 1024,
            page_token: None,
            include_generated: false,
        });
        let result = server.workspace_symbol_search(params).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_code_actions_tool_with_params() {
        let server = create_test_server();
        let params = Parameters(CodeActionsParams {
            file_path: "/test/file.rs".to_string(),
            start_line: 10,
            start_character: 5,
            end_line: 10,
            end_character: 15,
            kind_filter: None,
        });
        let result = server.get_code_actions(params).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_prepare_call_hierarchy_tool_with_params() {
        let server = create_test_server();
        let params = Parameters(CallHierarchyPrepareParams {
            file_path: "/test/file.rs".to_string(),
            line: 10,
            character: 5,
            project_id: None,
            page_token: None,
            symbol_handle: None,
        });
        let result = server.prepare_call_hierarchy(params).await;
        assert!(result.is_err());
    }
    #[tokio::test]
    async fn page_token_does_not_require_coordinates() {
        let server = create_test_server();
        let result = server
            .prepare_call_hierarchy(Parameters(CallHierarchyPrepareParams {
                file_path: String::new(),
                line: 0,
                character: 0,
                project_id: Some("missing".to_owned()),
                page_token: Some("mcpls-deferred:///missing".to_owned()),
                symbol_handle: None,
            }))
            .await;

        let error = result.unwrap_err().to_string();
        assert!(!error.contains("file_path, line, and character"));
    }

    #[tokio::test]
    async fn test_incoming_calls_tool_with_params() {
        let server = create_test_server();
        let item = serde_json::json!({
            "name": "test_function",
            "kind": 12,
            "uri": "file:///test/file.rs",
            "range": {
                "start": {"line": 0, "character": 0},
                "end": {"line": 0, "character": 10}
            },
            "selectionRange": {
                "start": {"line": 0, "character": 0},
                "end": {"line": 0, "character": 10}
            }
        });
        let params = Parameters(CallHierarchyCallsParams {
            item: Some(item),
            project_id: None,
            symbol_handle: None,
            limits: crate::bridge::SemanticResultLimits::default(),
        });
        let result = server.get_incoming_calls(params).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_outgoing_calls_tool_with_params() {
        let server = create_test_server();
        let item = serde_json::json!({
            "name": "test_function",
            "kind": 12,
            "uri": "file:///test/file.rs",
            "range": {
                "start": {"line": 0, "character": 0},
                "end": {"line": 0, "character": 10}
            },
            "selectionRange": {
                "start": {"line": 0, "character": 0},
                "end": {"line": 0, "character": 10}
            }
        });
        let params = Parameters(CallHierarchyCallsParams {
            item: Some(item),
            project_id: None,
            symbol_handle: None,
            limits: crate::bridge::SemanticResultLimits::default(),
        });
        let result = server.get_outgoing_calls(params).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_incoming_calls_routes_registered_items_to_project_actor() {
        let project_root = TempDir::new().unwrap();
        let file_path = project_root.path().join("src.rs");
        std::fs::write(&file_path, "fn main() {}\n").unwrap();

        let subscriptions = Arc::new(ResourceSubscriptions::new());
        let registry = ProjectRegistry::new(2);
        registry
            .add(ProjectIdentity::new(
                ProjectId::new("project").unwrap(),
                CanonicalRoot::new(project_root.path()).unwrap(),
            ))
            .await
            .unwrap();
        let server = McplsServer::new_with_registry(subscriptions, registry);
        let item = serde_json::json!({
            "name": "test_function",
            "kind": 12,
            "uri": crate::bridge::path_to_uri(&file_path).unwrap().to_string(),
            "range": {"start": {"line": 1, "character": 1}, "end": {"line": 1, "character": 10}},
            "selectionRange": {"start": {"line": 1, "character": 1}, "end": {"line": 1, "character": 10}}
        });

        let result = server
            .get_incoming_calls(Parameters(CallHierarchyCallsParams {
                item: Some(item),
                project_id: None,
                symbol_handle: None,
                limits: crate::bridge::SemanticResultLimits::default(),
            }))
            .await;
        let error = result.unwrap_err().to_string();
        assert!(!error.contains("outside workspace"), "{error}");
    }

    #[tokio::test]
    async fn test_outgoing_calls_routes_registered_items_to_project_actor() {
        let project_root = TempDir::new().unwrap();
        let file_path = project_root.path().join("src.rs");
        std::fs::write(&file_path, "fn main() {}\n").unwrap();

        let subscriptions = Arc::new(ResourceSubscriptions::new());
        let registry = ProjectRegistry::new(2);
        registry
            .add(ProjectIdentity::new(
                ProjectId::new("project").unwrap(),
                CanonicalRoot::new(project_root.path()).unwrap(),
            ))
            .await
            .unwrap();
        let server = McplsServer::new_with_registry(subscriptions, registry);
        let item = serde_json::json!({
            "name": "test_function",
            "kind": 12,
            "uri": crate::bridge::path_to_uri(&file_path).unwrap().to_string(),
            "range": {"start": {"line": 1, "character": 1}, "end": {"line": 1, "character": 10}},
            "selectionRange": {"start": {"line": 1, "character": 1}, "end": {"line": 1, "character": 10}}
        });

        let result = server
            .get_outgoing_calls(Parameters(CallHierarchyCallsParams {
                item: Some(item),
                project_id: None,
                symbol_handle: None,
                limits: crate::bridge::SemanticResultLimits::default(),
            }))
            .await;
        let error = result.unwrap_err().to_string();
        assert!(!error.contains("outside workspace"), "{error}");
    }

    #[tokio::test]
    async fn test_cached_diagnostics_tool_rejects_unregistered_paths() {
        use std::fs;

        use tempfile::TempDir;

        let server = create_test_server();

        let temp_dir = TempDir::new().unwrap();
        let test_file = temp_dir.path().join("test.rs");
        fs::write(&test_file, "fn main() {}").unwrap();

        let params = Parameters(CachedDiagnosticsParams {
            file_path: test_file.to_str().unwrap().to_string(),
            options: DiagnosticOptions::default(),
        });

        let result = server.get_cached_diagnostics(params).await;
        let error = result.unwrap_err().to_string();
        assert!(error.contains("path is not registered"), "{error}");
    }

    #[tokio::test]
    async fn test_cached_diagnostics_tool_nonexistent_file() {
        let server = create_test_server();
        let params = Parameters(CachedDiagnosticsParams {
            file_path: "/nonexistent/file.rs".to_string(),
            options: DiagnosticOptions::default(),
        });

        let result = server.get_cached_diagnostics(params).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_server_logs_tool_with_default_params() {
        let server = create_test_server_with_project().await;
        let params = Parameters(ServerLogsParams {
            project_id: "project".to_string(),
            limit: 50,
            min_level: None,
        });

        let result = server.get_server_logs(params).await;
        assert!(result.is_ok());

        let json_str = result.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        assert!(parsed.get("logs").is_some());
    }

    #[tokio::test]
    async fn test_server_logs_tool_with_error_level() {
        let server = create_test_server_with_project().await;
        let params = Parameters(ServerLogsParams {
            project_id: "project".to_string(),
            limit: 10,
            min_level: Some("error".to_string()),
        });

        let result = server.get_server_logs(params).await;
        assert!(result.is_ok());

        let json_str = result.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        let logs = parsed.get("logs").unwrap().as_array().unwrap();
        assert_eq!(logs.len(), 0);
    }

    #[tokio::test]
    async fn test_server_logs_tool_with_warning_level() {
        let server = create_test_server_with_project().await;
        let params = Parameters(ServerLogsParams {
            project_id: "project".to_string(),
            limit: 100,
            min_level: Some("warning".to_string()),
        });

        let result = server.get_server_logs(params).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_server_logs_tool_with_info_level() {
        let server = create_test_server_with_project().await;
        let params = Parameters(ServerLogsParams {
            project_id: "project".to_string(),
            limit: 50,
            min_level: Some("info".to_string()),
        });

        let result = server.get_server_logs(params).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_server_logs_tool_with_debug_level() {
        let server = create_test_server_with_project().await;
        let params = Parameters(ServerLogsParams {
            project_id: "project".to_string(),
            limit: 20,
            min_level: Some("debug".to_string()),
        });

        let result = server.get_server_logs(params).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_server_logs_tool_with_invalid_level() {
        let server = create_test_server_with_project().await;
        let params = Parameters(ServerLogsParams {
            project_id: "project".to_string(),
            limit: 10,
            min_level: Some("invalid_level".to_string()),
        });

        let result = server.get_server_logs(params).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_server_logs_tool_with_zero_limit() {
        let server = create_test_server_with_project().await;
        let params = Parameters(ServerLogsParams {
            project_id: "project".to_string(),
            limit: 0,
            min_level: None,
        });

        let result = server.get_server_logs(params).await;
        assert!(result.is_ok());

        let json_str = result.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        let logs = parsed.get("logs").unwrap().as_array().unwrap();
        assert_eq!(logs.len(), 0);
    }

    #[tokio::test]
    async fn test_server_messages_tool_with_default_params() {
        let server = create_test_server_with_project().await;
        let params = Parameters(ServerMessagesParams {
            project_id: "project".to_string(),
            limit: 20,
        });

        let result = server.get_server_messages(params).await;
        assert!(result.is_ok());

        let json_str = result.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        assert!(parsed.get("messages").is_some());
    }

    #[tokio::test]
    async fn test_server_messages_tool_with_custom_limit() {
        let server = create_test_server_with_project().await;
        let params = Parameters(ServerMessagesParams {
            project_id: "project".to_string(),
            limit: 5,
        });

        let result = server.get_server_messages(params).await;
        assert!(result.is_ok());

        let json_str = result.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        let messages = parsed.get("messages").unwrap().as_array().unwrap();
        assert_eq!(messages.len(), 0);
    }

    #[tokio::test]
    async fn test_server_messages_tool_with_zero_limit() {
        let server = create_test_server_with_project().await;
        let params = Parameters(ServerMessagesParams {
            project_id: "project".to_string(),
            limit: 0,
        });

        let result = server.get_server_messages(params).await;
        assert!(result.is_ok());

        let json_str = result.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        let messages = parsed.get("messages").unwrap().as_array().unwrap();
        assert_eq!(messages.len(), 0);
    }

    #[tokio::test]
    async fn test_server_messages_tool_with_large_limit() {
        let server = create_test_server_with_project().await;
        let params = Parameters(ServerMessagesParams {
            project_id: "project".to_string(),
            limit: 1000,
        });

        let result = server.get_server_messages(params).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_get_signature_help_tool_with_params() {
        let server = create_test_server();
        let params = Parameters(SignatureHelpParams {
            file_path: "/test/file.rs".to_string(),
            line: 10,
            character: 5,
        });

        let result = server.get_signature_help(params).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_go_to_implementation_tool_with_params() {
        let server = create_test_server();
        let params = Parameters(GoToImplementationParams {
            file_path: "/test/file.rs".to_string(),
            line: 10,
            character: 5,
            project_id: None,
            symbol_handle: None,
        });

        let result = server.go_to_implementation(params).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_go_to_type_definition_tool_with_params() {
        let server = create_test_server();
        let params = Parameters(GoToTypeDefinitionParams {
            file_path: "/test/file.rs".to_string(),
            line: 10,
            character: 5,
            project_id: None,
            symbol_handle: None,
        });

        let result = server.go_to_type_definition(params).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_get_inlay_hints_tool_with_params() {
        let server = create_test_server();
        let params = Parameters(InlayHintsParams {
            file_path: "/test/file.rs".to_string(),
            start_line: 1,
            start_character: 1,
            end_line: 10,
            end_character: 1,
        });

        let result = server.get_inlay_hints(params).await;
        assert!(result.is_err());
    }

    // ------------------------------------------------------------------
    // Resource handler tests (logic-level, avoiding rmcp::service::RequestContext
    // which requires a live Peer with private fields)
    // ------------------------------------------------------------------

    /// `list_resources` has no documents for a fresh project registry.
    #[tokio::test]
    async fn test_list_resources_returns_empty_when_no_open_documents() {
        let server = create_test_server();
        let empty = server
            .context
            .project_registry
            .open_document_paths()
            .await
            .unwrap()
            .is_empty();
        assert!(empty);
    }

    /// `parse_uri` rejects `file://` scheme — ensures `read_resource` would return an error.
    #[test]
    fn test_read_resource_rejects_file_scheme() {
        let result = parse_uri("file:///some/file.rs");
        assert!(result.is_err());
    }

    /// `parse_uri` rejects `https://` scheme.
    #[test]
    fn test_subscribe_rejects_https_scheme() {
        let result = parse_uri("https://evil.com/file.rs");
        assert!(result.is_err());
    }

    /// `validate_path` rejects a non-existent path (canonicalize fails).
    #[tokio::test]
    async fn test_validate_path_rejects_nonexistent_path() {
        use std::path::Path;

        let translator = Arc::new(Mutex::new(Translator::new()));
        let result = {
            let t = translator.lock().await;
            t.validate_path(Path::new("/this/path/does/not/exist/at/all.rs"))
        };
        assert!(result.is_err());
    }

    /// subscribe cap enforced: after `MAX_SUBSCRIPTIONS` entries, the next call returns `Err`.
    #[tokio::test]
    async fn test_subscription_cap_enforced_in_handler_context() {
        use crate::bridge::resources::MAX_SUBSCRIPTIONS;

        let subscriptions = Arc::new(ResourceSubscriptions::new());
        for i in 0..MAX_SUBSCRIPTIONS {
            subscriptions
                .subscribe(format!("lsp-diagnostics:///file{i}.rs"))
                .await
                .unwrap();
        }
        let over = subscriptions
            .subscribe("lsp-diagnostics:///overflow.rs".to_string())
            .await;
        assert!(over.is_err());
    }

    /// unsubscribing a URI that was never subscribed is a no-op (returns `false`, not an error).
    #[tokio::test]
    async fn test_unsubscribe_nonexistent_is_noop() {
        let subscriptions = Arc::new(ResourceSubscriptions::new());
        let removed = subscriptions
            .unsubscribe("lsp-diagnostics:///nonexistent.rs")
            .await;
        assert!(!removed);
    }

    /// Server capabilities advertise resources support.
    #[tokio::test]
    async fn test_server_capabilities_include_resources() {
        let server = create_test_server();
        let info = server.get_info();
        assert!(info.capabilities.resources.is_some());
    }

    #[test]
    fn mutating_apply_tools_advertise_destructive_annotations() {
        for tool in [
            McplsServer::workspace_edit_apply_tool_tool_attr(),
            McplsServer::rename_apply_tool_tool_attr(),
            McplsServer::format_apply_tool_tool_attr(),
            McplsServer::code_action_apply_tool_tool_attr(),
        ] {
            let Some(annotations) = tool.annotations else {
                panic!("mutating tool annotations");
            };
            assert_eq!(annotations.read_only_hint, Some(false));
            assert_eq!(annotations.destructive_hint, Some(true));
            assert_eq!(annotations.idempotent_hint, Some(true));
        }
    }

    async fn approval_fixture() -> (TempDir, McplsServer, String) {
        let root = TempDir::new().unwrap();
        let file = root.path().join("src.rs");
        std::fs::write(&file, "before\n").unwrap();
        let registry = ProjectRegistry::new(2);
        let actor = registry
            .add(ProjectIdentity::new(
                ProjectId::new("project").unwrap(),
                CanonicalRoot::new(root.path()).unwrap(),
            ))
            .await
            .unwrap();
        let plan = EditPlan::new(
            "project".to_owned(),
            vec![FileSnapshot::from_contents(
                file,
                SnapshotSource::Disk,
                None,
                "before\n",
                "after\n",
            )],
            vec!["replace src.rs".to_owned()],
            true,
            std::time::Duration::from_secs(60),
        );
        let plan_id = plan.id().as_str().to_owned();
        let plan_handle = PlanId::parse(plan_id.clone()).unwrap();
        actor.store_edit_plan(plan).await.unwrap();
        let server =
            McplsServer::new_with_registry(Arc::new(ResourceSubscriptions::new()), registry);
        server.context.remember_plan(plan_handle).await;
        (root, server, plan_id)
    }

    #[tokio::test]
    async fn mutating_apply_first_round_requests_approval_without_writing() {
        let (root, server, plan_id) = approval_fixture().await;
        let response = server
            .workspace_edit_apply_tool(
                Parameters(WorkspaceEditApplyParams {
                    project_id: "project".to_owned(),
                    plan_id,
                    wait_timeout_ms: None,
                }),
                RequestState(None),
                ToolInputResponses(None),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let CallToolResponse::InputRequired(result) = response else {
            panic!("first apply round must request input");
        };
        assert!(result.request_state.is_some());
        assert!(
            result
                .input_requests
                .as_ref()
                .unwrap()
                .contains_key(APPROVAL_INPUT_ID)
        );
        let summary = result
            .meta
            .as_ref()
            .and_then(|meta| meta.0.get("approvalSummary"))
            .unwrap();
        assert_eq!(summary["affected_file_count"], 1);
        assert_eq!(summary["operations"][0], "replace src.rs");
        assert_eq!(
            std::fs::read_to_string(root.path().join("src.rs")).unwrap(),
            "before\n"
        );
    }

    #[tokio::test]
    async fn mutating_apply_acceptance_applies_once_and_replay_is_rejected() {
        let (root, server, plan_id) = approval_fixture().await;
        let params = || {
            Parameters(WorkspaceEditApplyParams {
                project_id: "project".to_owned(),
                plan_id: plan_id.clone(),
                wait_timeout_ms: None,
            })
        };
        let first = server
            .workspace_edit_apply_tool(
                params(),
                RequestState(None),
                ToolInputResponses(None),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let CallToolResponse::InputRequired(first) = first else {
            panic!("first apply round must request input");
        };
        let state = first.request_state.unwrap();
        let mut responses = InputResponses::new();
        responses.insert(
            APPROVAL_INPUT_ID.to_owned(),
            serde_json::json!({
                "action": "accept",
                "content": {"approved": true}
            }),
        );
        let applied = server
            .workspace_edit_apply_tool(
                params(),
                RequestState(Some(state.clone())),
                ToolInputResponses(Some(responses.clone())),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(matches!(applied, CallToolResponse::Complete(_)));
        assert_eq!(
            std::fs::read_to_string(root.path().join("src.rs")).unwrap(),
            "after\n"
        );

        let replay = server
            .workspace_edit_apply_tool(
                params(),
                RequestState(Some(state)),
                ToolInputResponses(Some(responses)),
                CancellationToken::new(),
            )
            .await;
        assert!(replay.is_err());
        assert_eq!(
            std::fs::read_to_string(root.path().join("src.rs")).unwrap(),
            "after\n"
        );
    }

    #[tokio::test]
    async fn mutating_apply_decline_and_tamper_leave_files_unchanged() {
        let (root, server, plan_id) = approval_fixture().await;
        let params = || {
            Parameters(WorkspaceEditApplyParams {
                project_id: "project".to_owned(),
                plan_id: plan_id.clone(),
                wait_timeout_ms: None,
            })
        };
        let first = server
            .workspace_edit_apply_tool(
                params(),
                RequestState(None),
                ToolInputResponses(None),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let CallToolResponse::InputRequired(first) = first else {
            panic!("first apply round must request input");
        };
        let state = first.request_state.unwrap();
        let mut decline = InputResponses::new();
        decline.insert(
            APPROVAL_INPUT_ID.to_owned(),
            serde_json::json!({"action": "decline"}),
        );
        assert!(
            server
                .workspace_edit_apply_tool(
                    params(),
                    RequestState(Some(state.clone())),
                    ToolInputResponses(Some(decline)),
                    CancellationToken::new(),
                )
                .await
                .is_err()
        );
        assert_eq!(
            std::fs::read_to_string(root.path().join("src.rs")).unwrap(),
            "before\n"
        );

        let tampered = format!("{}x", &state[..state.len() - 1]);
        let mut accept = InputResponses::new();
        accept.insert(
            APPROVAL_INPUT_ID.to_owned(),
            serde_json::json!({
                "action": "accept",
                "content": {"approved": true}
            }),
        );
        assert!(
            server
                .workspace_edit_apply_tool(
                    params(),
                    RequestState(Some(tampered)),
                    ToolInputResponses(Some(accept)),
                    CancellationToken::new(),
                )
                .await
                .is_err()
        );
        assert_eq!(
            std::fs::read_to_string(root.path().join("src.rs")).unwrap(),
            "before\n"
        );
    }

    #[tokio::test]
    async fn mutating_apply_without_elicitation_uses_the_approved_tool_call() {
        let (root, server, plan_id) = approval_fixture().await;
        server.context.set_client_capabilities(None, true);
        let result = server
            .workspace_edit_apply_tool(
                Parameters(WorkspaceEditApplyParams {
                    project_id: "project".to_owned(),
                    plan_id,
                    wait_timeout_ms: None,
                }),
                RequestState(None),
                ToolInputResponses(None),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(matches!(result, CallToolResponse::Complete(_)));
        assert_eq!(
            std::fs::read_to_string(root.path().join("src.rs")).unwrap(),
            "after\n"
        );
    }

    #[tokio::test]
    async fn mutating_apply_rejects_malformed_response_and_stale_snapshot() {
        let (root, server, plan_id) = approval_fixture().await;
        let params = || {
            Parameters(WorkspaceEditApplyParams {
                project_id: "project".to_owned(),
                plan_id: plan_id.clone(),
                wait_timeout_ms: None,
            })
        };
        let first = server
            .workspace_edit_apply_tool(
                params(),
                RequestState(None),
                ToolInputResponses(None),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let CallToolResponse::InputRequired(first) = first else {
            panic!("first apply round must request input");
        };
        let state = first.request_state.unwrap();
        let mut malformed = InputResponses::new();
        malformed.insert(
            APPROVAL_INPUT_ID.to_owned(),
            serde_json::json!({"action": "accept", "content": {}}),
        );
        assert!(
            server
                .workspace_edit_apply_tool(
                    params(),
                    RequestState(Some(state.clone())),
                    ToolInputResponses(Some(malformed)),
                    CancellationToken::new(),
                )
                .await
                .is_err()
        );
        std::fs::write(root.path().join("src.rs"), "changed\n").unwrap();
        let mut accepted = InputResponses::new();
        accepted.insert(
            APPROVAL_INPUT_ID.to_owned(),
            serde_json::json!({
                "action": "accept",
                "content": {"approved": true}
            }),
        );
        let stale_retry = server
            .workspace_edit_apply_tool(
                params(),
                RequestState(Some(state)),
                ToolInputResponses(Some(accepted)),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let CallToolResponse::Complete(stale_retry) = stale_retry else {
            panic!("stale apply should return a structured conflict result");
        };
        assert_eq!(stale_retry.is_error, Some(false));
        assert_eq!(
            stale_retry
                .structured_content
                .as_ref()
                .and_then(|value| value.get("status")),
            Some(&serde_json::Value::String("conflict".to_owned()))
        );
        server.context.set_client_capabilities(None, false);
        let replay = server
            .workspace_edit_apply_tool(
                params(),
                RequestState(None),
                ToolInputResponses(None),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let CallToolResponse::Complete(replay) = replay else {
            panic!("terminal conflict should be replayable");
        };
        assert_eq!(
            replay
                .structured_content
                .as_ref()
                .and_then(|value| value.get("status")),
            Some(&serde_json::Value::String("conflict".to_owned()))
        );
        assert_eq!(
            std::fs::read_to_string(root.path().join("src.rs")).unwrap(),
            "changed\n"
        );
    }

    #[tokio::test]
    async fn mutating_apply_concurrent_retries_join_one_commit() {
        let (root, server, plan_id) = approval_fixture().await;
        let first = server
            .workspace_edit_apply_tool(
                Parameters(WorkspaceEditApplyParams {
                    project_id: "project".to_owned(),
                    plan_id: plan_id.clone(),
                    wait_timeout_ms: None,
                }),
                RequestState(None),
                ToolInputResponses(None),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let CallToolResponse::InputRequired(first) = first else {
            panic!("first apply round must request input");
        };
        let state = first.request_state.unwrap();
        let mut accepted = InputResponses::new();
        accepted.insert(
            APPROVAL_INPUT_ID.to_owned(),
            serde_json::json!({
                "action": "accept",
                "content": {"approved": true}
            }),
        );
        let params = || {
            Parameters(WorkspaceEditApplyParams {
                project_id: "project".to_owned(),
                plan_id: plan_id.clone(),
                wait_timeout_ms: None,
            })
        };
        let (left, right) = tokio::join!(
            server.workspace_edit_apply_tool(
                params(),
                RequestState(Some(state.clone())),
                ToolInputResponses(Some(accepted.clone())),
                CancellationToken::new(),
            ),
            server.workspace_edit_apply_tool(
                params(),
                RequestState(Some(state)),
                ToolInputResponses(Some(accepted)),
                CancellationToken::new(),
            )
        );
        assert!(left.is_ok());
        assert!(right.is_ok());
        assert_eq!(
            std::fs::read_to_string(root.path().join("src.rs")).unwrap(),
            "after\n"
        );
    }
}
