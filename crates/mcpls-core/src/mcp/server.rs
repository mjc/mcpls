//! MCP server implementation using rmcp.
//!
//! This module provides the MCP server that exposes LSP capabilities
//! as MCP tools using the rmcp SDK.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    Implementation, ListResourcesResult, RawResource, ReadResourceRequestParams,
    ReadResourceResult, ResourceContents, ResourceUpdatedNotificationParam, ServerCapabilities,
    ServerInfo, SubscribeRequestParams, UnsubscribeRequestParams,
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
    HoverParams, InlayHintsParams, ProjectAddParams, ProjectIdParams, ProjectListParams,
    ProjectLspCapabilitiesParams, ReferencesParams, RenameParams, RenamePreviewParams,
    ServerLogsParams, ServerMessagesParams, SignatureHelpParams, SubscriptionListParams,
    WorkspaceEditApplyParams, WorkspaceEditPreviewParams, WorkspaceSymbolParams,
};
#[cfg(test)]
use crate::bridge::Translator;
use crate::bridge::resources::make_uri;
use crate::bridge::{PositionEncoding, ResourceSubscriptions};
use crate::edit_plan::PlanId;
use crate::edit_preview::PreviewArtifact;
use crate::project::AppliedEditPlan;
use crate::project::{
    CanonicalRoot, GitRepositoryIdentity, ProjectEventRecord, ProjectEventSnapshot, ProjectHandle,
    ProjectId, ProjectIdentity, ProjectQueuePressure, ProjectRegistry, ProjectServerCapability,
    ProjectState, ProjectStatusCounts, ProjectStatusSummary,
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
    serde_json::json!({
        "project_id": project_id,
        "plan_id": result.plan_id.as_str(),
        "committed_files": result.committed_files,
        "operations": result.operations,
        "unified_diff": result.unified_diff,
    })
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
    serde_json::json!({
        "project_id": project_id,
        "plan_id": result.plan.id().as_str(),
        "unified_diff": result.plan.unified_diff(),
        "affected_files": result.affected_files,
        "operations": result.plan.operations(),
        "preconditions": preconditions,
        "conflicts": result.conflicts,
        "unsupported": result.unsupported,
        "safe_to_apply": result.plan.safe_to_apply(),
    })
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

    async fn actor_for_project(&self, value: String) -> Result<ProjectHandle, McpError> {
        let project_id = parse_project_id(value)?;
        self.context
            .project_registry
            .actor_for_project(&project_id)
            .await
            .map_err(|error| McpError::invalid_params(error.to_string(), None))
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
    ) -> Result<ReadResourceResult, McpError> {
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
        Ok(ReadResourceResult::new(vec![ResourceContents::text(
            json, uri,
        )]))
    }

    async fn read_project_events_resource(
        &self,
        project_id: ProjectId,
        cursor: Option<u64>,
        uri: String,
    ) -> Result<ReadResourceResult, McpError> {
        let actor = self
            .context
            .project_registry
            .actor_for_project(&project_id)
            .await
            .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
        let snapshot = actor.event_snapshot(cursor);
        let json = encode_json(&project_events_json(&project_id, &snapshot))?;
        Ok(ReadResourceResult::new(vec![ResourceContents::text(
            json, uri,
        )]))
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
            .apply_edit_plan(id, plan_id)
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
    #[tool(description = "Remove a registered project and stop its actor.")]
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
        let actor = self.actor_for_project(project_id).await?;
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
        let actor = self.actor_for_project(project_id).await?;
        encode_tool_result(actor.server_logs(limit, min_level).await)
    }

    /// Get recent LSP server messages.
    #[tool(
        description = "Recent server messages (showMessage notifications). User-facing prompts and status updates."
    )]
    async fn get_server_messages(
        &self,
        Parameters(ServerMessagesParams { project_id, limit }): Parameters<ServerMessagesParams>,
    ) -> Result<String, McpError> {
        let actor = self.actor_for_project(project_id).await?;
        encode_tool_result(actor.server_messages(limit).await)
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
                let raw = RawResource::new(uri, format!("{project_id}:{name}"))
                    .with_mime_type("application/json")
                    .with_description(format!("LSP diagnostics for {project_id}:{name}"));
                Some(rmcp::model::Annotated::new(raw, None))
            })
            .collect();

        let projects = self.context.project_registry.list().await;
        resources.extend(projects.iter().map(|identity| {
            let project_id = identity.id().clone();
            let uri = project_status_resource_uri(&project_id);
            let raw = RawResource::new(uri, format!("{project_id}:status"))
                .with_mime_type("application/json")
                .with_description(format!("Lifecycle status for project {project_id}"));
            rmcp::model::Annotated::new(raw, None)
        }));
        resources.extend(projects.iter().map(|identity| {
            let project_id = identity.id().clone();
            let uri = project_events_resource_uri(&project_id);
            let raw = RawResource::new(uri, format!("{project_id}:events"))
                .with_mime_type("application/json")
                .with_description(format!("Ordered project events for {project_id}"));
            rmcp::model::Annotated::new(raw, None)
        }));

        Ok(ListResourcesResult::with_all_items(resources))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: rmcp::service::RequestContext<RoleServer>,
    ) -> Result<ReadResourceResult, McpError> {
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

        Ok(ReadResourceResult::new(vec![ResourceContents::text(
            json,
            request.uri,
        )]))
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
    use super::*;
    use crate::bridge::resources::parse_uri;
    use crate::edit_plan::{EditPlan, FileSnapshot, SnapshotSource};
    use tempfile::TempDir;

    fn create_test_server() -> McplsServer {
        let subscriptions = Arc::new(ResourceSubscriptions::new());
        McplsServer::new(subscriptions)
    }

    #[test]
    fn server_constructor_does_not_require_a_global_translator() {
        let subscriptions = Arc::new(ResourceSubscriptions::new());
        let _server = McplsServer::new(subscriptions);
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
        let events: serde_json::Value = serde_json::to_value(events).unwrap();
        let event_text = events["contents"][0]["text"].as_str().unwrap();
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
    #[tokio::test]
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
        fs::write(
            &lsp,
            r##"#!/usr/bin/env python3
import json
import os
import pathlib
import sys

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
        send({"jsonrpc": "2.0", "id": message["id"], "result": {
            "capabilities": {"positionEncoding": "utf-8"}
        }})
        send({"jsonrpc": "2.0", "method": "experimental/serverStatus",
              "params": {"health": "ok", "quiescent": True}})
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
    elif method == "shutdown":
        send({"jsonrpc": "2.0", "id": message["id"], "result": None})
        break
"##,
        )
        .unwrap();
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

        let stale = server
            .format_apply(Parameters(WorkspaceEditApplyParams {
                project_id: project_id.as_str().to_string(),
                plan_id: format["plan_id"].as_str().unwrap().to_string(),
            }))
            .await;
        assert!(stale.is_err());
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
            "uri": crate::bridge::path_to_uri(&file_path).to_string(),
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
            "uri": crate::bridge::path_to_uri(&file_path).to_string(),
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
