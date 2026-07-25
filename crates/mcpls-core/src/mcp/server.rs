//! MCP server implementation using rmcp.
//!
//! This module provides the MCP server that exposes LSP capabilities
//! as MCP tools using the rmcp SDK.

use std::path::PathBuf;
use std::sync::Arc;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    Implementation, ListResourcesResult, RawResource, ReadResourceRequestParams,
    ReadResourceResult, ResourceContents, ServerCapabilities, ServerInfo, SubscribeRequestParams,
    UnsubscribeRequestParams,
};
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler, tool, tool_handler, tool_router};
use serde::Serialize;
use tokio::sync::Mutex;

use super::handlers::HandlerContext;
use super::tools::{
    CachedDiagnosticsParams, CallHierarchyCallsParams, CallHierarchyPrepareParams,
    CodeActionApplyParams, CodeActionListParams, CodeActionPreviewParams, CodeActionsParams,
    CompletionsParams, DefinitionParams, DiagnosticsParams, DocumentSymbolsParams,
    FormatDocumentParams, FormatPreviewParams, GoToImplementationParams, GoToTypeDefinitionParams,
    HoverParams, InlayHintsParams, ProjectAddParams, ProjectIdParams, ProjectListParams,
    ReferencesParams, RenameParams, RenamePreviewParams, ServerLogsParams, ServerMessagesParams,
    SignatureHelpParams, WorkspaceEditApplyParams, WorkspaceEditPreviewParams,
    WorkspaceSymbolParams,
};
use crate::bridge::resources::{make_uri, parse_uri};
use crate::bridge::{PositionEncoding, ResourceSubscriptions, Translator};
use crate::edit_plan::PlanId;
use crate::edit_preview::PreviewArtifact;
use crate::project::AppliedEditPlan;
use crate::project::{
    CanonicalRoot, GitRepositoryIdentity, ProjectHandle, ProjectId, ProjectIdentity,
    ProjectRegistry, ProjectState,
};

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

fn project_state_json(identity: &ProjectIdentity, state: &ProjectState) -> serde_json::Value {
    serde_json::json!({
        "project_id": identity.id().as_str(),
        "root": identity.root().as_path(),
        "repository_root": identity.repository_identity().map(GitRepositoryIdentity::common_dir),
        "status": format!("{:?}", state.status()),
        "last_error": state.last_error(),
        "configured_language_servers": state.runtime().configured_language_ids(),
        "active_language_servers": state.runtime().active_language_ids(),
        "open_document_count": state.open_document_count(),
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
    async fn actor_for_project(&self, value: String) -> Result<ProjectHandle, McpError> {
        let project_id = parse_project_id(value)?;
        self.context
            .project_registry
            .actor_for_project(&project_id)
            .await
            .map_err(|error| McpError::invalid_params(error.to_string(), None))
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
        encode_json(&preview_artifact_json(&artifact, id.as_str()))
    }

    async fn apply_project_plan(
        &self,
        id: &ProjectId,
        plan_id: PlanId,
    ) -> Result<String, McpError> {
        let result = self
            .context
            .project_registry
            .apply_edit_plan(id, plan_id)
            .await
            .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
        encode_json(&applied_edit_plan_json(&result, id.as_str()))
    }

    /// Create a new MCP server with the given translator and subscriptions.
    #[must_use]
    pub fn new(
        translator: Arc<Mutex<Translator>>,
        subscriptions: Arc<ResourceSubscriptions>,
    ) -> Self {
        let context = Arc::new(HandlerContext::new(translator, subscriptions));
        Self { context }
    }

    /// Create a server with an explicitly shared project registry.
    #[must_use]
    pub fn new_with_registry(
        translator: Arc<Mutex<Translator>>,
        subscriptions: Arc<ResourceSubscriptions>,
        project_registry: ProjectRegistry,
    ) -> Self {
        let context = Arc::new(HandlerContext::with_registry(
            translator,
            subscriptions,
            project_registry,
        ));
        Self { context }
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
            config: _,
        }): Parameters<ProjectAddParams>,
    ) -> Result<String, McpError> {
        let id = parse_project_id(project_id)?;
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
            .add(identity.clone())
            .await
            .map_err(|error| McpError::internal_error(error.to_string(), None))?;
        let state = actor
            .query()
            .await
            .map_err(|error| McpError::internal_error(error.to_string(), None))?;
        encode_json(&project_state_json(&identity, &state))
    }

    /// Activate a registered project and wait for its language servers to load.
    #[tool(
        description = "Activate a registered project. Blocks until its applicable language servers finish loading and are ready for code intelligence."
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
        encode_json(&project_state_json(&identity, &state))
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
                    "repository_root": project.repository_identity().map(GitRepositoryIdentity::common_dir),
                })
            })
            .collect();
        encode_json(&result)
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
        encode_json(&project_state_json(&identity, &state))
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
        description = "Preview a project-scoped LSP WorkspaceEdit. The result includes a plan ID, unified diff, affected files, preconditions, conflicts, unsupported operations, and explicit safety state."
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
        description = "Preview an LSP rename as a project-owned workspace edit plan. Apply the returned plan with workspace_edit_apply."
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
        description = "Preview LSP document formatting as a project-owned workspace edit plan. Apply the returned plan with workspace_edit_apply."
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

    /// Apply a previously previewed, project-owned workspace edit plan.
    #[tool(
        description = "Apply one previously previewed workspace edit plan by its project ID and opaque plan ID. Plans are single-use and are revalidated before any file is replaced."
    )]
    async fn workspace_edit_apply(
        &self,
        Parameters(WorkspaceEditApplyParams {
            project_id,
            plan_id,
        }): Parameters<WorkspaceEditApplyParams>,
    ) -> Result<String, McpError> {
        let id = parse_project_id(project_id.clone())?;
        let plan_id = PlanId::parse(plan_id)
            .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
        self.apply_project_plan(&id, plan_id).await
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
        encode_json(&project_state_json(&identity, &state))
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
        encode_json(&project_state_json(&identity, &state))
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
    #[tool(description = "Preview a code action using its project-scoped reference.")]
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
        encode_json(&preview_artifact_json(&result, id.as_str()))
    }

    /// Apply a previously previewed code action plan.
    #[tool(description = "Apply a code action preview plan for a project.")]
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
        let resources: Vec<_> = open_documents
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

        Ok(ListResourcesResult::with_all_items(resources))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: rmcp::service::RequestContext<RoleServer>,
    ) -> Result<ReadResourceResult, McpError> {
        let path =
            parse_uri(&request.uri).map_err(|e| McpError::invalid_params(e.to_string(), None))?;

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
        _context: rmcp::service::RequestContext<RoleServer>,
    ) -> Result<(), McpError> {
        let path =
            parse_uri(&request.uri).map_err(|e| McpError::invalid_params(e.to_string(), None))?;

        let actor = self
            .context
            .required_actor_for_path(&path)
            .await
            .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
        actor
            .validate_path(path.display().to_string())
            .await
            .map_err(|error| McpError::invalid_params(error.to_string(), None))?;

        // TODO(S3): If diagnostics are already cached for this URI, emit a synthetic
        // notify_resource_updated so clients subscribing after initial workspace indexing
        // don't have to wait for the next LSP push. Requires peer access from HandlerContext.
        // Track as follow-up issue.
        self.context
            .subscriptions
            .subscribe(request.uri)
            .await
            .map_err(|e| McpError::invalid_params(e, None))?;

        Ok(())
    }

    async fn unsubscribe(
        &self,
        request: UnsubscribeRequestParams,
        _context: rmcp::service::RequestContext<RoleServer>,
    ) -> Result<(), McpError> {
        // Parse the URI for consistency with subscribe validation.
        parse_uri(&request.uri).map_err(|e| McpError::invalid_params(e.to_string(), None))?;
        self.context.subscriptions.unsubscribe(&request.uri).await;
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
    use crate::edit_plan::{EditPlan, FileSnapshot, SnapshotSource};
    use tempfile::TempDir;

    fn create_test_server() -> McplsServer {
        let translator = Arc::new(Mutex::new(Translator::new()));
        let subscriptions = Arc::new(ResourceSubscriptions::new());
        McplsServer::new(translator, subscriptions)
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
        let translator = Arc::new(Mutex::new(Translator::new()));
        let subscriptions = Arc::new(ResourceSubscriptions::new());
        let registry = ProjectRegistry::new(2);
        registry
            .add(ProjectIdentity::new(
                ProjectId::new("project").unwrap(),
                CanonicalRoot::new(".").unwrap(),
            ))
            .await
            .unwrap();
        McplsServer::new_with_registry(translator, subscriptions, registry)
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
        assert!(added.contains("demo"));
        let duplicate = server
            .project_add(Parameters(ProjectAddParams {
                project_id: "demo".to_string(),
                root: root.path().display().to_string(),
                config: Some(serde_json::json!({"ignored": true})),
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

        let translator = Arc::new(Mutex::new(Translator::new()));
        let subscriptions = Arc::new(ResourceSubscriptions::new());
        let server = McplsServer::new_with_registry(translator, subscriptions, registry);
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

        let server = McplsServer::new_with_registry(
            Arc::new(Mutex::new(Translator::new())),
            Arc::new(ResourceSubscriptions::new()),
            registry,
        );
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
        let server = McplsServer::new_with_registry(
            Arc::new(Mutex::new(Translator::new())),
            Arc::new(ResourceSubscriptions::new()),
            registry,
        );
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
        let translator = Arc::new(Mutex::new(translator));
        let subscriptions = Arc::new(ResourceSubscriptions::new());
        let registry = ProjectRegistry::with_translator_template(2, template);
        let server = McplsServer::new_with_registry(translator, subscriptions, registry);

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
        let translator = Arc::new(Mutex::new(Translator::new()));
        let subscriptions = Arc::new(ResourceSubscriptions::new());
        let registry = crate::project::ProjectRegistry::new(2);
        let server = McplsServer::new_with_registry(translator, subscriptions, registry.clone());
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
        let unrelated_root = TempDir::new().unwrap();
        let file_path = project_root.path().join("src.rs");
        std::fs::write(&file_path, "fn main() {}\n").unwrap();

        let mut translator = Translator::new();
        translator.set_workspace_roots(vec![unrelated_root.path().to_path_buf()]);
        let translator = Arc::new(Mutex::new(translator));
        let subscriptions = Arc::new(ResourceSubscriptions::new());
        let registry = ProjectRegistry::new(2);
        registry
            .add(ProjectIdentity::new(
                ProjectId::new("project").unwrap(),
                CanonicalRoot::new(project_root.path()).unwrap(),
            ))
            .await
            .unwrap();
        let server = McplsServer::new_with_registry(translator, subscriptions, registry);

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
        let unrelated_root = TempDir::new().unwrap();
        let file_path = project_root.path().join("src.rs");
        std::fs::write(&file_path, "fn main() {}\n").unwrap();

        let mut translator = Translator::new();
        translator.set_workspace_roots(vec![unrelated_root.path().to_path_buf()]);
        let translator = Arc::new(Mutex::new(translator));
        let subscriptions = Arc::new(ResourceSubscriptions::new());
        let registry = ProjectRegistry::new(2);
        registry
            .add(ProjectIdentity::new(
                ProjectId::new("project").unwrap(),
                CanonicalRoot::new(project_root.path()).unwrap(),
            ))
            .await
            .unwrap();
        let server = McplsServer::new_with_registry(translator, subscriptions, registry);

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
        let unrelated_root = TempDir::new().unwrap();
        let file_path = project_root.path().join("src.rs");
        std::fs::write(&file_path, "fn main() {}\n").unwrap();

        let mut translator = Translator::new();
        translator.set_workspace_roots(vec![unrelated_root.path().to_path_buf()]);
        let translator = Arc::new(Mutex::new(translator));
        let subscriptions = Arc::new(ResourceSubscriptions::new());
        let registry = ProjectRegistry::new(2);
        registry
            .add(ProjectIdentity::new(
                ProjectId::new("project").unwrap(),
                CanonicalRoot::new(project_root.path()).unwrap(),
            ))
            .await
            .unwrap();
        let server = McplsServer::new_with_registry(translator, subscriptions, registry);

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
        let unrelated_root = TempDir::new().unwrap();
        let file_path = project_root.path().join("src.rs");
        std::fs::write(&file_path, "fn main() {}\n").unwrap();

        let mut translator = Translator::new();
        translator.set_workspace_roots(vec![unrelated_root.path().to_path_buf()]);
        let translator = Arc::new(Mutex::new(translator));
        let subscriptions = Arc::new(ResourceSubscriptions::new());
        let registry = ProjectRegistry::new(2);
        registry
            .add(ProjectIdentity::new(
                ProjectId::new("project").unwrap(),
                CanonicalRoot::new(project_root.path()).unwrap(),
            ))
            .await
            .unwrap();
        let server = McplsServer::new_with_registry(translator, subscriptions, registry);

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
        let unrelated_root = TempDir::new().unwrap();
        let file_path = project_root.path().join("src.rs");
        std::fs::write(&file_path, "fn main() {}\n").unwrap();

        let mut translator = Translator::new();
        translator.set_workspace_roots(vec![unrelated_root.path().to_path_buf()]);
        let translator = Arc::new(Mutex::new(translator));
        let subscriptions = Arc::new(ResourceSubscriptions::new());
        let registry = ProjectRegistry::new(2);
        registry
            .add(ProjectIdentity::new(
                ProjectId::new("project").unwrap(),
                CanonicalRoot::new(project_root.path()).unwrap(),
            ))
            .await
            .unwrap();
        let server = McplsServer::new_with_registry(translator, subscriptions, registry);

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
        let unrelated_root = TempDir::new().unwrap();
        let file_path = project_root.path().join("src.rs");
        std::fs::write(&file_path, "fn main() {}\n").unwrap();

        let mut translator = Translator::new();
        translator.set_workspace_roots(vec![unrelated_root.path().to_path_buf()]);
        let translator = Arc::new(Mutex::new(translator));
        let subscriptions = Arc::new(ResourceSubscriptions::new());
        let registry = ProjectRegistry::new(2);
        registry
            .add(ProjectIdentity::new(
                ProjectId::new("project").unwrap(),
                CanonicalRoot::new(project_root.path()).unwrap(),
            ))
            .await
            .unwrap();
        let server = McplsServer::new_with_registry(translator, subscriptions, registry);

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
        let unrelated_root = TempDir::new().unwrap();
        let file_path = project_root.path().join("src.rs");
        std::fs::write(&file_path, "fn main() {}\n").unwrap();

        let mut translator = Translator::new();
        translator.set_workspace_roots(vec![unrelated_root.path().to_path_buf()]);
        let translator = Arc::new(Mutex::new(translator));
        let subscriptions = Arc::new(ResourceSubscriptions::new());
        let registry = ProjectRegistry::new(2);
        registry
            .add(ProjectIdentity::new(
                ProjectId::new("project").unwrap(),
                CanonicalRoot::new(project_root.path()).unwrap(),
            ))
            .await
            .unwrap();
        let server = McplsServer::new_with_registry(translator, subscriptions, registry);

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
        let unrelated_root = TempDir::new().unwrap();
        let file_path = project_root.path().join("src.rs");
        std::fs::write(&file_path, "fn main() {}\n").unwrap();

        let mut translator = Translator::new();
        translator.set_workspace_roots(vec![unrelated_root.path().to_path_buf()]);
        let translator = Arc::new(Mutex::new(translator));
        let subscriptions = Arc::new(ResourceSubscriptions::new());
        let registry = ProjectRegistry::new(2);
        registry
            .add(ProjectIdentity::new(
                ProjectId::new("project").unwrap(),
                CanonicalRoot::new(project_root.path()).unwrap(),
            ))
            .await
            .unwrap();
        let server = McplsServer::new_with_registry(translator, subscriptions, registry);

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
        let unrelated_root = TempDir::new().unwrap();
        let file_path = project_root.path().join("src.rs");
        std::fs::write(&file_path, "fn main() {}\n").unwrap();

        let mut translator = Translator::new();
        translator.set_workspace_roots(vec![unrelated_root.path().to_path_buf()]);
        let translator = Arc::new(Mutex::new(translator));
        let subscriptions = Arc::new(ResourceSubscriptions::new());
        let registry = ProjectRegistry::new(2);
        registry
            .add(ProjectIdentity::new(
                ProjectId::new("project").unwrap(),
                CanonicalRoot::new(project_root.path()).unwrap(),
            ))
            .await
            .unwrap();
        let server = McplsServer::new_with_registry(translator, subscriptions, registry);

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
        let unrelated_root = TempDir::new().unwrap();
        let file_path = project_root.path().join("src.rs");
        std::fs::write(&file_path, "fn main() {}\n").unwrap();

        let mut translator = Translator::new();
        translator.set_workspace_roots(vec![unrelated_root.path().to_path_buf()]);
        let translator = Arc::new(Mutex::new(translator));
        let subscriptions = Arc::new(ResourceSubscriptions::new());
        let registry = ProjectRegistry::new(2);
        registry
            .add(ProjectIdentity::new(
                ProjectId::new("project").unwrap(),
                CanonicalRoot::new(project_root.path()).unwrap(),
            ))
            .await
            .unwrap();
        let server = McplsServer::new_with_registry(translator, subscriptions, registry);

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
        let unrelated_root = TempDir::new().unwrap();
        let file_path = project_root.path().join("src.rs");
        std::fs::write(&file_path, "fn main() {}\n").unwrap();

        let mut translator = Translator::new();
        translator.set_workspace_roots(vec![unrelated_root.path().to_path_buf()]);
        let translator = Arc::new(Mutex::new(translator));
        let subscriptions = Arc::new(ResourceSubscriptions::new());
        let registry = ProjectRegistry::new(2);
        registry
            .add(ProjectIdentity::new(
                ProjectId::new("project").unwrap(),
                CanonicalRoot::new(project_root.path()).unwrap(),
            ))
            .await
            .unwrap();
        let server = McplsServer::new_with_registry(translator, subscriptions, registry);

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
        let unrelated_root = TempDir::new().unwrap();
        let file_path = project_root.path().join("src.rs");
        std::fs::write(&file_path, "fn main() {}\n").unwrap();

        let mut translator = Translator::new();
        translator.set_workspace_roots(vec![unrelated_root.path().to_path_buf()]);
        let translator = Arc::new(Mutex::new(translator));
        let subscriptions = Arc::new(ResourceSubscriptions::new());
        let registry = ProjectRegistry::new(2);
        registry
            .add(ProjectIdentity::new(
                ProjectId::new("project").unwrap(),
                CanonicalRoot::new(project_root.path()).unwrap(),
            ))
            .await
            .unwrap();
        let server = McplsServer::new_with_registry(translator, subscriptions, registry);

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
        let unrelated_root = TempDir::new().unwrap();
        let file_path = project_root.path().join("src.rs");
        std::fs::write(&file_path, "fn main() {}\n").unwrap();
        let mut translator = Translator::new();
        translator.set_workspace_roots(vec![unrelated_root.path().to_path_buf()]);
        let translator = Arc::new(Mutex::new(translator));
        let subscriptions = Arc::new(ResourceSubscriptions::new());
        let registry = ProjectRegistry::new(2);
        registry
            .add(ProjectIdentity::new(
                ProjectId::new("project").unwrap(),
                CanonicalRoot::new(project_root.path()).unwrap(),
            ))
            .await
            .unwrap();
        let server = McplsServer::new_with_registry(translator, subscriptions, registry);

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
        let unrelated_root = TempDir::new().unwrap();
        let file_path = project_root.path().join("src.rs");
        std::fs::write(&file_path, "fn main() {}\n").unwrap();
        let mut translator = Translator::new();
        translator.set_workspace_roots(vec![unrelated_root.path().to_path_buf()]);
        let translator = Arc::new(Mutex::new(translator));
        let subscriptions = Arc::new(ResourceSubscriptions::new());
        let registry = ProjectRegistry::new(2);
        registry
            .add(ProjectIdentity::new(
                ProjectId::new("project").unwrap(),
                CanonicalRoot::new(project_root.path()).unwrap(),
            ))
            .await
            .unwrap();
        let server = McplsServer::new_with_registry(translator, subscriptions, registry);

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
        let unrelated_root = TempDir::new().unwrap();
        let file_path = project_root.path().join("src.rs");
        std::fs::write(&file_path, "fn main() {}\n").unwrap();
        let mut translator = Translator::new();
        translator.set_workspace_roots(vec![unrelated_root.path().to_path_buf()]);
        let translator = Arc::new(Mutex::new(translator));
        let subscriptions = Arc::new(ResourceSubscriptions::new());
        let registry = ProjectRegistry::new(2);
        registry
            .add(ProjectIdentity::new(
                ProjectId::new("project").unwrap(),
                CanonicalRoot::new(project_root.path()).unwrap(),
            ))
            .await
            .unwrap();
        let server = McplsServer::new_with_registry(translator, subscriptions, registry);

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
        let unrelated_root = TempDir::new().unwrap();
        let file_path = project_root.path().join("src.rs");
        std::fs::write(&file_path, "fn main() {}\n").unwrap();

        let mut translator = Translator::new();
        translator.set_workspace_roots(vec![unrelated_root.path().to_path_buf()]);
        let translator = Arc::new(Mutex::new(translator));
        let subscriptions = Arc::new(ResourceSubscriptions::new());
        let registry = ProjectRegistry::new(2);
        registry
            .add(ProjectIdentity::new(
                ProjectId::new("project").unwrap(),
                CanonicalRoot::new(project_root.path()).unwrap(),
            ))
            .await
            .unwrap();
        let server = McplsServer::new_with_registry(translator, subscriptions, registry);
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
        let unrelated_root = TempDir::new().unwrap();
        let file_path = project_root.path().join("src.rs");
        std::fs::write(&file_path, "fn main() {}\n").unwrap();

        let mut translator = Translator::new();
        translator.set_workspace_roots(vec![unrelated_root.path().to_path_buf()]);
        let translator = Arc::new(Mutex::new(translator));
        let subscriptions = Arc::new(ResourceSubscriptions::new());
        let registry = ProjectRegistry::new(2);
        registry
            .add(ProjectIdentity::new(
                ProjectId::new("project").unwrap(),
                CanonicalRoot::new(project_root.path()).unwrap(),
            ))
            .await
            .unwrap();
        let server = McplsServer::new_with_registry(translator, subscriptions, registry);
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
