//! MCP server implementation using rmcp.
//!
//! This module provides the MCP server that exposes LSP capabilities
//! as MCP tools using the rmcp SDK.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    Implementation, ListResourcesResult, ReadResourceRequestParams, ReadResourceResponse,
    ReadResourceResult, Resource, ResourceContents, ResourceUpdatedNotificationParam,
    ServerCapabilities, ServerInfo, SubscribeRequestParams, UnsubscribeRequestParams,
};
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler, tool, tool_handler, tool_router};
use serde::Serialize;
#[cfg(test)]
use tokio::sync::Mutex;

use super::handlers::HandlerContext;
use super::session::{
    SessionResource, parse_session_resource_uri, project_events_resource_uri,
    project_status_resource_uri,
};
use super::tools::{
    CachedDiagnosticsParams, CallHierarchyCallsParams, CallHierarchyPrepareParams,
    CodeActionApplyParams, CodeActionListParams, CodeActionPreviewParams, CodeActionsParams,
    CompletionsParams, DefinitionParams, DiagnosticsParams, DocumentSymbolsParams,
    FormatDocumentParams, FormatPreviewParams, GoToImplementationParams, GoToTypeDefinitionParams,
    HoverParams, InlayHintsParams, MoveInlineModulePreviewParams, MoveItemPreviewParams,
    PathRenamePreviewParams, ProjectAddParams, ProjectIdParams, ProjectListParams,
    ProjectLspCapabilitiesParams, RangeFormatPreviewParams, ReferencesParams, RenameParams,
    RenamePreviewParams, SemanticPositionParams, ServerLogsParams, ServerMessagesParams,
    SignatureHelpParams, StructuralReplacePreviewParams, SubscriptionListParams,
    WorkspaceEditApplyParams, WorkspaceEditPreviewParams, WorkspaceSymbolParams,
};
#[cfg(test)]
use crate::bridge::Translator;
use crate::bridge::resources::make_uri;
use crate::bridge::{
    PositionEncoding, ResourceSubscriptions, SemanticDiscoveryKind, SupportedWorkspaceEdit,
};
use crate::edit_plan::PlanId;
use crate::edit_preview::PreviewArtifact;
use crate::project::AppliedEditPlan;
use crate::project::{
    CanonicalRoot, GitRepositoryIdentity, PathRenamePreview, PathRenameRequest, ProjectEventRecord,
    ProjectEventSnapshot, ProjectHandle, ProjectId, ProjectIdentity, ProjectQueuePressure,
    ProjectRegistry, ProjectServerCapability, ProjectState, ProjectStatusCounts,
    ProjectStatusSummary, StructuralDialect, StructuralPreview, StructuralReplaceRequest,
};
use crate::transport::{SessionManagerHandle, TransportSnapshot};

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

fn encode_json<T: Serialize>(value: &T) -> Result<String, McpError> {
    serde_json::to_string(value).map_err(|error| McpError::internal_error(error.to_string(), None))
}

fn encode_tool_result<T, E>(result: Result<T, E>) -> Result<String, McpError>
where
    T: Serialize,
    E: std::fmt::Display,
{
    result.map_or_else(
        |error| Err(McpError::internal_error(error.to_string(), None)),
        |value| encode_json(&value),
    )
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

#[derive(Serialize)]
struct ActorGroupState {
    group_id: usize,
    roots: Vec<PathBuf>,
}

#[derive(Serialize)]
struct ProjectLspCapabilitiesResponse {
    project_id: String,
    servers: Vec<ProjectServerCapability>,
}

#[derive(Debug, Clone, Serialize)]
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

#[derive(Serialize)]
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
        "resync_required": snapshot.resync_required(),
        "events": snapshot
            .events()
            .iter()
            .map(ProjectEventRecord::json_value)
            .collect::<Vec<_>>(),
    })
}

fn applied_edit_plan_json(result: &AppliedEditPlan, project_id: &str) -> serde_json::Value {
    let mut value = serde_json::json!({
        "project_id": project_id,
        "plan_id": result.plan_id.as_str(),
        "committed_files": result.committed_files,
        "operations": result.operations,
        "unified_diff": result.unified_diff,
    });
    if let Some(verification) = result.verification {
        value["verification"] = serde_json::json!(verification.as_str());
    }
    if !result.provider_synchronization.is_empty() {
        value["provider_synchronization"] = serde_json::json!(result.provider_synchronization);
        value["semantic_state"] = if result
            .provider_synchronization
            .iter()
            .all(|provider| provider.synchronized)
        {
            serde_json::Value::String("synchronized".to_string())
        } else {
            serde_json::Value::String("degraded".to_string())
        };
    }
    value
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
    value["matched_files"] = serde_json::json!(matched_files);
    value["matches"] = serde_json::json!(matches);
    if result.artifact.is_none() {
        value["unsupported"] = serde_json::json!(Vec::<String>::new());
    }
    value
}

fn path_rename_preview_json(result: &PathRenamePreview, project_id: &str) -> serde_json::Value {
    let mut value = preview_artifact_json(&result.artifact, project_id);
    value["semantic_providers"] = serde_json::json!(result.providers);
    value["semantic_provider_available"] = serde_json::json!(!result.providers.is_empty());
    value["semantic_edit_count"] = serde_json::json!(result.semantic_edit_count);
    value
}

/// MCP server that exposes LSP capabilities as tools.
pub struct McplsServer {
    context: Arc<HandlerContext>,
}

impl Clone for McplsServer {
    fn clone(&self) -> Self {
        self.for_session()
    }
}

#[tool_router]
impl McplsServer {
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
    ) -> Result<String, McpError> {
        let actor_group_roots = self
            .context
            .project_registry
            .actor_group_roots(project_id)
            .await
            .map_err(|error| McpError::internal_error(error.to_string(), None))?;
        let actor_groups = actor_group_states(actor_group_roots);
        encode_json(&project_state_json(identity, state, &actor_groups))
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
        Ok(ReadResourceResult::new(vec![ResourceContents::text(json, uri)]).into())
    }

    async fn read_project_events_resource(
        &self,
        project_id: ProjectId,
        cursor: Option<u64>,
        uri: String,
    ) -> Result<ReadResourceResponse, McpError> {
        let actor = self
            .context
            .project_registry
            .actor_for_project(&project_id)
            .await
            .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
        let snapshot = actor.event_snapshot(cursor);
        let json = encode_json(&project_events_json(&project_id, &snapshot))?;
        Ok(ReadResourceResult::new(vec![ResourceContents::text(json, uri)]).into())
    }

    async fn preview_project_edit(
        &self,
        id: &ProjectId,
        edit: lsp_types::WorkspaceEdit,
        encoding: PositionEncoding,
    ) -> Result<String, McpError> {
        let artifact = self
            .context
            .project_registry
            .preview_edit(id, edit, encoding)
            .await
            .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
        self.context.remember_plan(artifact.plan.id().clone()).await;
        encode_json(&preview_artifact_json(&artifact, id.as_str()))
    }

    async fn preview_supported_edit(
        &self,
        id: &ProjectId,
        result: SupportedWorkspaceEdit,
        encoding: PositionEncoding,
    ) -> Result<String, McpError> {
        let Some(edit) = result.edit else {
            return encode_json(&serde_json::json!({
                "project_id": id.as_str(),
                "supported": result.supported,
                "changed": false,
            }));
        };
        let artifact = self
            .context
            .project_registry
            .preview_edit(id, edit, encoding)
            .await
            .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
        self.context.remember_plan(artifact.plan.id().clone()).await;
        let mut value = preview_artifact_json(&artifact, id.as_str());
        value["supported"] = serde_json::Value::Bool(true);
        value["changed"] = serde_json::Value::Bool(true);
        encode_json(&value)
    }

    async fn semantic_discovery(
        &self,
        params: SemanticPositionParams,
        kind: SemanticDiscoveryKind,
    ) -> Result<String, McpError> {
        let id = parse_project_id(params.project_id)?;
        let actor = self
            .context
            .project_registry
            .actor_for_project(&id)
            .await
            .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
        let result = actor
            .semantic_discovery(params.file_path, params.line, params.character, kind)
            .await
            .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
        encode_json(&result)
    }

    async fn apply_project_plan(
        &self,
        id: &ProjectId,
        plan_id: PlanId,
    ) -> Result<String, McpError> {
        if !self.context.claim_plan(&plan_id).await {
            return Err(McpError::invalid_params(
                "edit plan is not owned by this MCP session",
                None,
            ));
        }
        let result = self
            .context
            .project_registry
            .apply_edit_plan_with_context(
                id,
                plan_id,
                Some(self.context.session_id().to_owned()),
                None,
            )
            .await
            .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
        encode_json(&applied_edit_plan_json(&result, id.as_str()))
    }

    async fn apply_project_plan_params(
        &self,
        params: WorkspaceEditApplyParams,
    ) -> Result<String, McpError> {
        let id = parse_project_id(params.project_id)?;
        let plan_id = PlanId::parse(params.plan_id)
            .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
        self.apply_project_plan(&id, plan_id).await
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
    ) -> Result<String, McpError> {
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
    ) -> Result<String, McpError> {
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
    #[tool(description = "List registered projects and their canonical roots.")]
    async fn project_list(
        &self,
        Parameters(_params): Parameters<ProjectListParams>,
    ) -> Result<String, McpError> {
        let projects = self.context.project_registry.list().await;
        let result: Vec<_> = projects
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
        encode_json(&result)
    }

    /// List resource subscriptions owned by this MCP session.
    #[tool(description = "List resource URIs subscribed by this MCP session.")]
    async fn subscription_list(
        &self,
        Parameters(_params): Parameters<SubscriptionListParams>,
    ) -> Result<String, McpError> {
        let subscriptions = self.context.subscriptions.sorted_snapshot().await;
        encode_json(&SubscriptionListResult { subscriptions })
    }

    /// Return a cheap process and project liveness snapshot.
    #[tool(description = "Return daemon liveness and non-blocking project lifecycle counts.")]
    async fn health(
        &self,
        Parameters(_params): Parameters<ProjectListParams>,
    ) -> Result<String, McpError> {
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
        Parameters(_params): Parameters<ProjectListParams>,
    ) -> Result<String, McpError> {
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
    ) -> Result<String, McpError> {
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
    ) -> Result<String, McpError> {
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
    ) -> Result<String, McpError> {
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
    ) -> Result<String, McpError> {
        let id = parse_project_id(project_id)?;
        let encoding = parse_position_encoding(position_encoding.as_deref())?;
        let actor = self
            .context
            .project_registry
            .actor_for_project(&id)
            .await
            .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
        let edit = actor
            .rename_workspace_edit(file_path, line, character, new_name)
            .await
            .map_err(|error| McpError::internal_error(error.to_string(), None))?
            .unwrap_or_default();
        self.preview_project_edit(&id, edit, encoding).await
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
        }): Parameters<FormatPreviewParams>,
    ) -> Result<String, McpError> {
        let id = parse_project_id(project_id)?;
        let encoding = parse_position_encoding(position_encoding.as_deref())?;
        let actor = self
            .context
            .project_registry
            .actor_for_project(&id)
            .await
            .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
        let edit = actor
            .format_workspace_edit(file_path, tab_size, insert_spaces)
            .await
            .map_err(|error| McpError::internal_error(error.to_string(), None))?
            .unwrap_or_default();
        self.preview_project_edit(&id, edit, encoding).await
    }

    /// Preview standard range formatting when the project server supports it.
    #[tool(
        description = "Preview capability-gated LSP range formatting as a session-owned edit plan. Unsupported servers return supported=false without creating a plan. Apply changed previews with workspace_edit_apply."
    )]
    async fn range_format_preview(
        &self,
        Parameters(params): Parameters<RangeFormatPreviewParams>,
    ) -> Result<String, McpError> {
        let id = parse_project_id(params.project_id)?;
        let encoding = parse_position_encoding(params.position_encoding.as_deref())?;
        let actor = self
            .context
            .project_registry
            .actor_for_project(&id)
            .await
            .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
        let result = actor
            .range_format_workspace_edit(
                params.file_path,
                (params.start_line, params.start_character),
                (params.end_line, params.end_character),
                params.tab_size,
                params.insert_spaces,
            )
            .await
            .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
        self.preview_supported_edit(&id, result, encoding).await
    }

    /// Preview rust-analyzer's syntax-aware item movement extension.
    #[tool(
        description = "Preview capability-gated rust-analyzer item movement up or down as a session-owned edit plan. No-op and unsupported responses create no plan. Snippet edits containing unresolved placeholders fail closed. Apply changed previews with workspace_edit_apply."
    )]
    async fn move_item_preview(
        &self,
        Parameters(params): Parameters<MoveItemPreviewParams>,
    ) -> Result<String, McpError> {
        let id = parse_project_id(params.project_id)?;
        let encoding = parse_position_encoding(params.position_encoding.as_deref())?;
        let actor = self
            .context
            .project_registry
            .actor_for_project(&id)
            .await
            .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
        let result = actor
            .move_item_workspace_edit(
                params.file_path,
                (params.start_line, params.start_character),
                (params.end_line, params.end_character),
                params.direction,
            )
            .await
            .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
        self.preview_supported_edit(&id, result, encoding).await
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
    ) -> Result<String, McpError> {
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
    ) -> Result<String, McpError> {
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
    ) -> Result<String, McpError> {
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
        encode_json(&structural_preview_json(&result, id.as_str()))
    }

    /// Apply a previously previewed, session-owned workspace edit plan.
    #[tool(
        description = "Apply one workspace edit plan previewed by this MCP session, by project ID and opaque plan ID. Plans are single-use and are revalidated before any file is replaced."
    )]
    async fn workspace_edit_apply(
        &self,
        Parameters(params): Parameters<WorkspaceEditApplyParams>,
    ) -> Result<String, McpError> {
        self.apply_project_plan_params(params).await
    }

    /// Apply a rename preview through the generic workspace-edit transaction.
    #[tool(
        description = "Apply a rename plan returned by rename_preview. Plans are single-use and revalidated before any file is replaced."
    )]
    async fn rename_apply(
        &self,
        Parameters(params): Parameters<WorkspaceEditApplyParams>,
    ) -> Result<String, McpError> {
        self.apply_project_plan_params(params).await
    }

    /// Apply a formatting preview through the generic workspace-edit transaction.
    #[tool(
        description = "Apply a formatting plan returned by format_preview. Plans are single-use and revalidated before any file is replaced."
    )]
    async fn format_apply(
        &self,
        Parameters(params): Parameters<WorkspaceEditApplyParams>,
    ) -> Result<String, McpError> {
        self.apply_project_plan_params(params).await
    }

    /// Restart the language-server actor for one project.
    #[tool(description = "Restart the language servers for a registered project.")]
    async fn project_restart_lsp(
        &self,
        Parameters(ProjectIdParams { project_id }): Parameters<ProjectIdParams>,
    ) -> Result<String, McpError> {
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

    /// Refresh one project actor's observable state.
    #[tool(description = "Refresh the status of a registered project.")]
    async fn project_refresh(
        &self,
        Parameters(ProjectIdParams { project_id }): Parameters<ProjectIdParams>,
    ) -> Result<String, McpError> {
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
        description = "Type and documentation info at position. Returns signatures, docs, and inferred types for symbols."
    )]
    async fn get_hover(
        &self,
        Parameters(HoverParams {
            file_path,
            line,
            character,
        }): Parameters<HoverParams>,
    ) -> Result<String, McpError> {
        let actor = self
            .context
            .required_actor_for_path(&file_path)
            .await
            .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
        let result = actor
            .hover(file_path, line, character)
            .await
            .map_err(|error| error.to_string());

        match result {
            Ok(value) => serde_json::to_string(&value)
                .map_err(|e| McpError::internal_error(format!("Serialization error: {e}"), None)),
            Err(e) => Err(McpError::internal_error(e, None)),
        }
    }

    /// Get the definition location of a symbol.
    #[tool(
        description = "Definition location of symbol at position. Returns file path, line, and character where declared."
    )]
    async fn get_definition(
        &self,
        Parameters(DefinitionParams {
            file_path,
            line,
            character,
        }): Parameters<DefinitionParams>,
    ) -> Result<String, McpError> {
        let actor = self
            .context
            .required_actor_for_path(&file_path)
            .await
            .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
        let result = actor
            .definition(file_path, line, character)
            .await
            .map_err(|error| error.to_string());

        match result {
            Ok(value) => serde_json::to_string(&value)
                .map_err(|e| McpError::internal_error(format!("Serialization error: {e}"), None)),
            Err(e) => Err(McpError::internal_error(e, None)),
        }
    }

    /// Get the declaration location, distinct from a symbol's definition.
    #[tool(
        description = "Project-scoped standard LSP declaration lookup. Returns supported=false when the active server does not advertise textDocument/declaration."
    )]
    async fn get_declaration(
        &self,
        Parameters(params): Parameters<SemanticPositionParams>,
    ) -> Result<String, McpError> {
        self.semantic_discovery(params, SemanticDiscoveryKind::Declaration)
            .await
    }

    /// Locate the Rust module containing a position.
    #[tool(
        description = "Project-scoped rust-analyzer parent-module navigation as read-only location data. Returns supported=false when unavailable."
    )]
    async fn get_parent_module(
        &self,
        Parameters(params): Parameters<SemanticPositionParams>,
    ) -> Result<String, McpError> {
        self.semantic_discovery(params, SemanticDiscoveryKind::ParentModule)
            .await
    }

    /// Locate child Rust modules declared at a position.
    #[tool(
        description = "Project-scoped rust-analyzer child-module navigation as bounded read-only location data. Returns supported=false when unavailable."
    )]
    async fn get_child_modules(
        &self,
        Parameters(params): Parameters<SemanticPositionParams>,
    ) -> Result<String, McpError> {
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
    ) -> Result<String, McpError> {
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
    ) -> Result<String, McpError> {
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
    ) -> Result<String, McpError> {
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
    ) -> Result<String, McpError> {
        self.semantic_discovery(params, SemanticDiscoveryKind::RelatedTests)
            .await
    }

    /// Find all references to a symbol.
    #[tool(
        description = "All references to symbol at position. Returns locations across workspace where symbol is used."
    )]
    async fn get_references(
        &self,
        Parameters(ReferencesParams {
            file_path,
            line,
            character,
            include_declaration,
        }): Parameters<ReferencesParams>,
    ) -> Result<String, McpError> {
        let actor = self
            .context
            .required_actor_for_path(&file_path)
            .await
            .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
        let result = actor
            .references(file_path, line, character, include_declaration)
            .await
            .map_err(|error| error.to_string());

        match result {
            Ok(value) => serde_json::to_string(&value)
                .map_err(|e| McpError::internal_error(format!("Serialization error: {e}"), None)),
            Err(e) => Err(McpError::internal_error(e, None)),
        }
    }

    /// Get diagnostics for a file.
    #[tool(
        description = "Diagnostics for a file. Returns errors, warnings, and hints with severity and location."
    )]
    async fn get_diagnostics(
        &self,
        Parameters(DiagnosticsParams { file_path }): Parameters<DiagnosticsParams>,
    ) -> Result<String, McpError> {
        let actor = self
            .context
            .required_actor_for_path(&file_path)
            .await
            .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
        let result = actor
            .diagnostics(file_path)
            .await
            .map_err(|error| error.to_string());

        match result {
            Ok(value) => serde_json::to_string(&value)
                .map_err(|e| McpError::internal_error(format!("Serialization error: {e}"), None)),
            Err(e) => Err(McpError::internal_error(e, None)),
        }
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
    ) -> Result<String, McpError> {
        let actor = self
            .context
            .required_actor_for_path(&file_path)
            .await
            .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
        let result = actor
            .rename(file_path, line, character, new_name)
            .await
            .map_err(|error| error.to_string());

        match result {
            Ok(value) => serde_json::to_string(&value)
                .map_err(|e| McpError::internal_error(format!("Serialization error: {e}"), None)),
            Err(e) => Err(McpError::internal_error(e, None)),
        }
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
    ) -> Result<String, McpError> {
        let actor = self
            .context
            .required_actor_for_path(&file_path)
            .await
            .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
        let result = actor
            .completions(file_path, line, character, trigger)
            .await
            .map_err(|error| error.to_string());

        match result {
            Ok(value) => serde_json::to_string(&value)
                .map_err(|e| McpError::internal_error(format!("Serialization error: {e}"), None)),
            Err(e) => Err(McpError::internal_error(e, None)),
        }
    }

    /// Get all symbols in a document.
    #[tool(
        description = "Symbols in a file. Returns hierarchical outline with functions, classes, structs, and locations."
    )]
    async fn get_document_symbols(
        &self,
        Parameters(DocumentSymbolsParams { file_path }): Parameters<DocumentSymbolsParams>,
    ) -> Result<String, McpError> {
        let actor = self
            .context
            .required_actor_for_path(&file_path)
            .await
            .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
        let result = actor
            .document_symbols(file_path)
            .await
            .map_err(|error| error.to_string());

        match result {
            Ok(value) => serde_json::to_string(&value)
                .map_err(|e| McpError::internal_error(format!("Serialization error: {e}"), None)),
            Err(e) => Err(McpError::internal_error(e, None)),
        }
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
    ) -> Result<String, McpError> {
        let actor = self
            .context
            .required_actor_for_path(&file_path)
            .await
            .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
        let result = actor
            .format_document(file_path, tab_size, insert_spaces)
            .await
            .map_err(|error| error.to_string());

        match result {
            Ok(value) => serde_json::to_string(&value)
                .map_err(|e| McpError::internal_error(format!("Serialization error: {e}"), None)),
            Err(e) => Err(McpError::internal_error(e, None)),
        }
    }

    /// Search for symbols across the workspace.
    #[tool(
        description = "Search workspace symbols by name. Supports partial matching and fuzzy search."
    )]
    async fn workspace_symbol_search(
        &self,
        Parameters(WorkspaceSymbolParams {
            project_id,
            query,
            kind_filter,
            limit,
        }): Parameters<WorkspaceSymbolParams>,
    ) -> Result<String, McpError> {
        let id = parse_project_id(project_id)?;
        if let Err(error) = self.context.project_registry.activate(&id).await {
            tracing::warn!(
                project_id = %id,
                error = %error,
                "workspace symbol activation failed; trying degraded lookup"
            );
        }
        let actor = self
            .context
            .project_registry
            .actor_for_project(&id)
            .await
            .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
        let result = actor
            .workspace_symbol(query, kind_filter, limit)
            .await
            .map_err(|error| error.to_string());

        match result {
            Ok(value) => serde_json::to_string(&value)
                .map_err(|e| McpError::internal_error(format!("Serialization error: {e}"), None)),
            Err(e) => Err(McpError::internal_error(e, None)),
        }
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
    ) -> Result<String, McpError> {
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

        match result {
            Ok(value) => serde_json::to_string(&value)
                .map_err(|e| McpError::internal_error(format!("Serialization error: {e}"), None)),
            Err(e) => Err(McpError::internal_error(e, None)),
        }
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
    ) -> Result<String, McpError> {
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
    ) -> Result<String, McpError> {
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
    #[tool(description = "Apply a code action preview plan owned by this MCP session.")]
    async fn code_action_apply(
        &self,
        Parameters(CodeActionApplyParams {
            project_id,
            plan_id,
        }): Parameters<CodeActionApplyParams>,
    ) -> Result<String, McpError> {
        let id = parse_project_id(project_id)?;
        let plan_id = PlanId::parse(plan_id)
            .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
        self.apply_project_plan(&id, plan_id).await
    }

    /// Prepare call hierarchy at a position.
    #[tool(
        description = "Prepare call hierarchy at position. Returns callable items for incoming/outgoing call analysis."
    )]
    async fn prepare_call_hierarchy(
        &self,
        Parameters(CallHierarchyPrepareParams {
            file_path,
            line,
            character,
        }): Parameters<CallHierarchyPrepareParams>,
    ) -> Result<String, McpError> {
        let actor = self
            .context
            .required_actor_for_path(&file_path)
            .await
            .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
        let result = actor
            .prepare_call_hierarchy(file_path, line, character)
            .await
            .map_err(|error| error.to_string());

        match result {
            Ok(value) => serde_json::to_string(&value)
                .map_err(|e| McpError::internal_error(format!("Serialization error: {e}"), None)),
            Err(e) => Err(McpError::internal_error(e, None)),
        }
    }

    /// Get incoming calls (callers).
    #[tool(
        description = "Functions calling the specified item. Takes call hierarchy item, returns all callers."
    )]
    async fn get_incoming_calls(
        &self,
        Parameters(CallHierarchyCallsParams { item }): Parameters<CallHierarchyCallsParams>,
    ) -> Result<String, McpError> {
        let path = call_hierarchy_item_path(&item)?;
        let actor = self
            .context
            .required_actor_for_path(&path)
            .await
            .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
        let result = actor
            .incoming_calls(item)
            .await
            .map_err(|error| error.to_string());

        encode_tool_result(result)
    }

    /// Get outgoing calls (callees).
    #[tool(
        description = "Functions called by the specified item. Takes call hierarchy item, returns all callees."
    )]
    async fn get_outgoing_calls(
        &self,
        Parameters(CallHierarchyCallsParams { item }): Parameters<CallHierarchyCallsParams>,
    ) -> Result<String, McpError> {
        let path = call_hierarchy_item_path(&item)?;
        let actor = self
            .context
            .required_actor_for_path(&path)
            .await
            .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
        let result = actor
            .outgoing_calls(item)
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
        Parameters(CachedDiagnosticsParams { file_path }): Parameters<CachedDiagnosticsParams>,
    ) -> Result<String, McpError> {
        let actor = self
            .context
            .required_actor_for_path(&file_path)
            .await
            .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
        let result = actor
            .cached_diagnostics(file_path)
            .await
            .map_err(|error| error.to_string());

        match result {
            Ok(value) => serde_json::to_string(&value)
                .map_err(|e| McpError::internal_error(format!("Serialization error: {e}"), None)),
            Err(e) => Err(McpError::internal_error(e, None)),
        }
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
    ) -> Result<String, McpError> {
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
    ) -> Result<String, McpError> {
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
    ) -> Result<String, McpError> {
        let id = parse_project_id(project_id)?;
        let servers = self
            .context
            .project_registry
            .server_capabilities(&id, language_id)
            .await
            .map_err(|error| McpError::internal_error(error.to_string(), None))?;
        encode_json(&ProjectLspCapabilitiesResponse {
            project_id: id.as_str().to_string(),
            servers,
        })
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
    ) -> Result<String, McpError> {
        let actor = self
            .context
            .required_actor_for_path(&file_path)
            .await
            .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
        let result = actor
            .signature_help(file_path, line, character)
            .await
            .map_err(|error| error.to_string());

        match result {
            Ok(value) => serde_json::to_string(&value)
                .map_err(|e| McpError::internal_error(format!("Serialization error: {e}"), None)),
            Err(e) => Err(McpError::internal_error(e, None)),
        }
    }

    /// Go to implementation locations.
    #[tool(
        description = "Implementation locations of trait method or interface member at position."
    )]
    async fn go_to_implementation(
        &self,
        Parameters(GoToImplementationParams {
            file_path,
            line,
            character,
        }): Parameters<GoToImplementationParams>,
    ) -> Result<String, McpError> {
        let actor = self
            .context
            .required_actor_for_path(&file_path)
            .await
            .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
        let result = actor
            .go_to_implementation(file_path, line, character)
            .await
            .map_err(|error| error.to_string());

        match result {
            Ok(value) => serde_json::to_string(&value)
                .map_err(|e| McpError::internal_error(format!("Serialization error: {e}"), None)),
            Err(e) => Err(McpError::internal_error(e, None)),
        }
    }

    /// Go to type definition location.
    #[tool(
        description = "Type definition location of expression at position. Distinct from go-to-definition for variable bindings."
    )]
    async fn go_to_type_definition(
        &self,
        Parameters(GoToTypeDefinitionParams {
            file_path,
            line,
            character,
        }): Parameters<GoToTypeDefinitionParams>,
    ) -> Result<String, McpError> {
        let actor = self
            .context
            .required_actor_for_path(&file_path)
            .await
            .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
        let result = actor
            .go_to_type_definition(file_path, line, character)
            .await
            .map_err(|error| error.to_string());

        match result {
            Ok(value) => serde_json::to_string(&value)
                .map_err(|e| McpError::internal_error(format!("Serialization error: {e}"), None)),
            Err(e) => Err(McpError::internal_error(e, None)),
        }
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
    ) -> Result<String, McpError> {
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

        match result {
            Ok(value) => serde_json::to_string(&value)
                .map_err(|e| McpError::internal_error(format!("Serialization error: {e}"), None)),
            Err(e) => Err(McpError::internal_error(e, None)),
        }
    }
}

#[tool_handler]
impl ServerHandler for McplsServer {
    async fn list_resources(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        _context: rmcp::service::RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        // TODO(critic-S5): paginate when max_documents == 0 (unlimited mode can produce
        // very large single-page responses that may exceed transport buffers).
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

        Ok(ListResourcesResult::with_all_items(resources))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: rmcp::service::RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, McpError> {
        let resource = parse_session_resource_uri(&request.uri)
            .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
        let path = match resource {
            SessionResource::ProjectStatus(project_id) => {
                return self
                    .read_project_status_resource(project_id, request.uri)
                    .await;
            }
            SessionResource::ProjectEvents { project_id, cursor } => {
                return self
                    .read_project_events_resource(project_id, cursor, request.uri)
                    .await;
            }
            SessionResource::Diagnostics(path) => path,
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

        Ok(ReadResourceResult::new(vec![ResourceContents::text(json, request.uri)]).into())
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
            SessionResource::Diagnostics(path) => path,
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
            SessionResource::ProjectStatus(_) | SessionResource::Diagnostics(_) => request.uri,
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
            concat!(
                "Universal MCP to LSP bridge. Exposes Language Server Protocol ",
                "capabilities as MCP tools for semantic code intelligence. ",
                "Supports hover, definition, references, diagnostics, rename, ",
                "completions, symbols, and formatting."
            )
            .to_string(),
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

    #[test]
    fn server_constructor_does_not_require_a_global_translator() {
        let subscriptions = Arc::new(ResourceSubscriptions::new());
        let _server = McplsServer::new(subscriptions);
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

        assert_eq!(value["diff_truncated"], true);
        assert_eq!(value["diff_files"][0]["additions"], 20_000);
        assert_eq!(value["diff_files"][0]["deletions"], 20_000);
        assert_eq!(value["operations"][0], "text src/huge.rs");
        assert_eq!(value["conflicts"][0], "conflict");
        assert_eq!(value["unsupported"][0], "unsupported");
        assert_eq!(value["preconditions"].as_array().unwrap().len(), 1);
        assert_eq!(value["safe_to_apply"], false);
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
        elif method == "textDocument/documentSymbol":
            if block_root and pathlib.Path.cwd() == block_root:
                entered.write_text("entered")
                while not release.exists():
                    time.sleep(0.001)
            send({"jsonrpc": "2.0", "id": message["id"], "result": []})
        elif method == "workspace/symbol":
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
        std::fs::write(&file, "fn main() {}\n").unwrap();
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
    fn document_symbols_params(path: &std::path::Path) -> Parameters<DocumentSymbolsParams> {
        Parameters(DocumentSymbolsParams {
            file_path: path.display().to_string(),
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

        server
            .workspace_symbol_search(Parameters(WorkspaceSymbolParams {
                project_id: "dormant".to_string(),
                query: "fixture".to_string(),
                kind_filter: None,
                limit: 20,
            }))
            .await
            .unwrap();
        let project_id = ProjectId::new("dormant").unwrap();
        wait_for_project_ready(&server.context.project_registry, &project_id).await;
        let result = server
            .workspace_symbol_search(Parameters(WorkspaceSymbolParams {
                project_id: "dormant".to_string(),
                query: "fixture".to_string(),
                kind_filter: None,
                limit: 20,
            }))
            .await
            .unwrap();
        let result: serde_json::Value = serde_json::from_str(&result).unwrap();

        assert_eq!(result["symbols"][0]["name"], "fixture_symbol");
        assert_eq!(std::fs::read_to_string(counter).unwrap(), "1");
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
        let registry = ProjectRegistry::with_translator_template(4, concurrency_template(config));
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
                query: "fixture_symbol".to_string(),
                kind_filter: None,
                limit: 20,
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
                .health(Parameters(ProjectListParams {}))
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
                .server_status(Parameters(ProjectListParams {}))
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
                .health(Parameters(ProjectListParams {}))
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
                .health(Parameters(ProjectListParams {}))
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
                .health(Parameters(ProjectListParams {}))
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(health["status"], "degraded");
        assert!(health["persistence"]["last_error"].is_string());
    }

    #[tokio::test]
    async fn workspace_edit_apply_consumes_project_owned_plan() {
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

        let events = server
            .read_project_events_resource(
                ProjectId::new("project").unwrap(),
                None,
                "mcpls-project-events:///project".to_string(),
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
        assert_eq!(event_payload["events"].as_array().unwrap().len(), 2);
        assert_eq!(event_payload["events"][0]["event"]["kind"], "files_changed");
        assert_eq!(event_payload["events"][1]["event"]["kind"], "edit_applied");

        let second = server
            .workspace_edit_apply(Parameters(WorkspaceEditApplyParams {
                project_id: "project".to_string(),
                plan_id,
            }))
            .await;
        assert!(second.is_err());
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
            }))
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(file).unwrap(),
            "fn main() { bar(1); bar(2); }\n"
        );
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
            }))
            .await
            .unwrap_err()
            .to_string();
        assert!(cross_session.contains("not owned by this MCP session"));

        server
            .workspace_edit_apply(Parameters(WorkspaceEditApplyParams {
                project_id: "project".to_string(),
                plan_id: result["plan_id"].as_str().unwrap().to_string(),
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
            }))
            .await
            .unwrap_err()
            .to_string();
        assert!(format.contains("project is not registered"), "{format}");

        let rename_apply = server
            .rename_apply(Parameters(WorkspaceEditApplyParams {
                project_id: "missing".to_string(),
                plan_id: "plan-1".to_string(),
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
        send({"jsonrpc": "2.0", "id": "watch-register", "method": "client/registerCapability",
              "params": {"registrations": [{
                  "id": "rust-files", "method": "workspace/didChangeWatchedFiles",
                  "registerOptions": {"watchers": [
                      {"globPattern": "**/*.rs", "kind": 7}
                  ]}
              }]}})
    elif method == "textDocument/rename":
        bump()
        uri = message["params"]["textDocument"]["uri"]
        sibling_uri = uri.replace("src.rs", "other.rs")
        edit = {"changes": {
            uri: [{"range": {"start": {"line": 0, "character": 0},
                              "end": {"line": 0, "character": 8}},
                   "newText": "new_name"}],
            sibling_uri: [{"range": {"start": {"line": 0, "character": 0},
                                      "end": {"line": 0, "character": 8}},
                           "newText": "new_name"}],
        }}
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
        path = pathlib.Path(urllib.parse.unquote(urllib.parse.urlparse(uri).path))
        send({"jsonrpc": "2.0", "id": message["id"],
              "result": [] if path.exists() else None})
    elif method == "workspace/didChangeWatchedFiles":
        if False:
            sys.exit(0)
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
            }))
            .await
            .unwrap();
        let format: serde_json::Value = serde_json::from_str(&format).unwrap();
        assert_eq!(fs::read_to_string(&counter).unwrap(), "2");
        server
            .format_apply(Parameters(WorkspaceEditApplyParams {
                project_id: project_id.as_str().to_string(),
                plan_id: format["plan_id"].as_str().unwrap().to_string(),
            }))
            .await
            .unwrap();
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
        let stale = server
            .workspace_edit_apply(Parameters(WorkspaceEditApplyParams {
                project_id: project_id.as_str().to_string(),
                plan_id: stale_preview["plan_id"].as_str().unwrap().to_string(),
            }))
            .await;
        assert!(stale.is_err());
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
        server
            .workspace_edit_apply(Parameters(WorkspaceEditApplyParams {
                project_id: project_id.as_str().to_string(),
                plan_id: fresh_range["plan_id"].as_str().unwrap().to_string(),
            }))
            .await
            .unwrap();
        assert_eq!(fs::read_to_string(&renamed).unwrap(), "ranged");
        let stale_range = server
            .workspace_edit_apply(Parameters(WorkspaceEditApplyParams {
                project_id: project_id.as_str().to_string(),
                plan_id: ranged["plan_id"].as_str().unwrap().to_string(),
            }))
            .await;
        assert!(stale_range.is_err());

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
            }))
            .await
            .unwrap();
        assert_eq!(fs::read_to_string(&unicode).unwrap(), "éranged\n");

        let semantic_position = || SemanticPositionParams {
            project_id: project_id.as_str().to_string(),
            file_path: renamed.display().to_string(),
            line: 1,
            character: 1,
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
            }))
            .await
            .unwrap();
        assert_eq!(fs::read_to_string(&renamed).unwrap(), "moved\n");

        let stale = server
            .format_apply(Parameters(WorkspaceEditApplyParams {
                project_id: project_id.as_str().to_string(),
                plan_id: format["plan_id"].as_str().unwrap().to_string(),
            }))
            .await;
        assert!(stale.is_err());
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
                .range_format_preview(Parameters(RangeFormatPreviewParams {
                    project_id: project_id.as_str().to_string(),
                    file_path: source.display().to_string(),
                    start_line: 1,
                    start_character: 1,
                    end_line: 1,
                    end_character: 3,
                    tab_size: 4,
                    insert_spaces: true,
                    position_encoding: None,
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
            }))
            .await
            .unwrap_err()
            .to_string();
        assert!(isolated.contains("outside workspace"), "{isolated}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn provider_exit_during_resource_sync_keeps_the_committed_apply_result() {
        use std::collections::HashMap;
        use std::fs;
        use std::os::unix::fs::PermissionsExt;

        let root = TempDir::new().unwrap();
        let source = root.path().join("src.rs");
        let destination = root.path().join("renamed.rs");
        let sibling = root.path().join("other.rs");
        let counter = root.path().join("request-count");
        fs::write(&source, "old_name\n").unwrap();
        fs::write(&sibling, "old_name();\n").unwrap();
        let lsp = root.path().join("fake-edit-lsp.py");
        fs::write(
            &lsp,
            FAKE_EDIT_LSP.replace(
                "if False:\n            sys.exit(0)",
                "if True:\n            sys.exit(0)",
            ),
        )
        .unwrap();
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
        assert_eq!(fs::read_to_string(destination).unwrap(), "old_name\n");
        assert_eq!(fs::read_to_string(sibling).unwrap(), "path_ref();\n");
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
        config.command = "/definitely/missing/rust-analyzer".to_string();
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
                include_declaration: false,
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
            }))
            .await;

        let error = result.unwrap_err().to_string();
        assert!(!error.contains("outside workspace"), "{error}");
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
            }))
            .await;

        let error = result.unwrap_err().to_string();
        assert!(!error.contains("outside workspace"), "{error}");
    }

    #[tokio::test]
    async fn test_cached_diagnostics_routes_registered_paths_to_project_actor() {
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
            .get_cached_diagnostics(Parameters(CachedDiagnosticsParams {
                file_path: file_path.display().to_string(),
            }))
            .await;

        let response = result.unwrap();
        assert_eq!(response, r#"{"diagnostics":[]}"#);
    }

    #[tokio::test]
    async fn test_definition_tool_with_params() {
        let server = create_test_server();
        let params = Parameters(DefinitionParams {
            file_path: "/test/file.rs".to_string(),
            line: 10,
            character: 5,
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
            include_declaration: false,
        });

        let result = server.get_references(params).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_diagnostics_tool_with_params() {
        let server = create_test_server();
        let params = Parameters(DiagnosticsParams {
            file_path: "/test/file.rs".to_string(),
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
            query: "User".to_string(),
            kind_filter: None,
            limit: 100,
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
        });
        let result = server.prepare_call_hierarchy(params).await;
        assert!(result.is_err());
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
        let params = Parameters(CallHierarchyCallsParams { item });
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
        let params = Parameters(CallHierarchyCallsParams { item });
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
            .get_incoming_calls(Parameters(CallHierarchyCallsParams { item }))
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
            .get_outgoing_calls(Parameters(CallHierarchyCallsParams { item }))
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
}
