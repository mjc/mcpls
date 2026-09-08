//! Project actor protocol, handles, dispatch, and lifecycle transitions.

#![allow(clippy::redundant_pub_crate)]

#[allow(clippy::wildcard_imports)]
use super::*;
#[allow(clippy::wildcard_imports)]
use super::{identity::*, registry::*, runtime::*, state::*};

/// Errors returned when a project actor cannot service a request.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ProjectActorError {
    /// The actor request channel has closed.
    #[error("project actor is closed")]
    Closed,
    /// The actor dropped a response before replying.
    #[error("project actor cancelled the request")]
    Cancelled,
    /// The actor operation failed after it started.
    #[error("project actor operation failed: {0}")]
    Operation(String),
}

#[derive(Clone)]
pub(super) struct ProjectRequestGate {
    accepting: std::sync::Arc<AtomicBool>,
    rejected: std::sync::Arc<Notify>,
}

impl ProjectRequestGate {
    pub(super) fn new() -> Self {
        Self {
            accepting: std::sync::Arc::new(AtomicBool::new(true)),
            rejected: std::sync::Arc::new(Notify::new()),
        }
    }

    pub(super) fn reject_new_work(&self) {
        self.accepting.store(false, Ordering::Release);
        self.rejected.notify_waiters();
    }

    pub(super) fn accept_new_work(&self) {
        self.accepting.store(true, Ordering::Release);
    }

    pub(super) fn is_accepting(&self) -> bool {
        self.accepting.load(Ordering::Acquire)
    }

    pub(super) async fn wait_for_rejection(&self) {
        self.rejected.notified().await;
    }
}

#[derive(Clone)]
pub(super) struct ProjectRequestSender {
    pub(super) sender: mpsc::Sender<ProjectRequest>,
    gate: ProjectRequestGate,
    pub(super) residency: Option<ProjectResidency>,
}

pub(super) struct ProjectRequestTiming {
    pub(super) queued_at: Instant,
    span: tracing::Span,
}

impl ProjectRequestTiming {
    pub(super) fn capture() -> Self {
        Self {
            queued_at: Instant::now(),
            span: tracing::Span::current(),
        }
    }
}

impl ProjectRequestSender {
    #[cfg(test)]
    pub(super) fn new(sender: mpsc::Sender<ProjectRequest>) -> Self {
        Self::with_gate(sender, None, ProjectRequestGate::new())
    }

    #[cfg(test)]
    pub(super) fn with_residency(
        sender: mpsc::Sender<ProjectRequest>,
        residency: ProjectResidency,
    ) -> Self {
        Self::with_gate(sender, Some(residency), ProjectRequestGate::new())
    }

    pub(super) const fn with_gate(
        sender: mpsc::Sender<ProjectRequest>,
        residency: Option<ProjectResidency>,
        gate: ProjectRequestGate,
    ) -> Self {
        Self {
            sender,
            gate,
            residency,
        }
    }

    pub(super) fn queue_pressure(&self) -> ProjectQueuePressure {
        ProjectQueuePressure {
            queued: self.sender.max_capacity() - self.sender.capacity(),
            capacity: self.sender.max_capacity(),
        }
    }

    pub(super) fn same_channel(&self, other: &Self) -> bool {
        self.sender.same_channel(&other.sender)
    }

    pub(super) fn reject_new_work(&self) {
        self.gate.reject_new_work();
    }

    pub(super) fn begin_shutdown(&self) {
        self.gate.reject_new_work();
    }

    pub(super) fn accept_new_work(&self) {
        self.gate.accept_new_work();
    }

    #[allow(clippy::result_large_err)]
    pub(super) async fn send(
        &self,
        mut request: ProjectRequest,
    ) -> Result<(), mpsc::error::SendError<ProjectRequest>> {
        if !self.gate.is_accepting() {
            return Err(mpsc::error::SendError(request));
        }

        request = ProjectRequest::Timed {
            request: Box::new(request),
            timing: ProjectRequestTiming::capture(),
        };

        if let Some(mode) = request.rust_residency_mode()
            && let Some(residency) = &self.residency
        {
            request = match mode {
                RustResidencyMode::Touch => residency.touch_request(request),
                RustResidencyMode::Resume | RustResidencyMode::Activate => {
                    residency.resident_request(request, mode).await
                }
            };
        }

        let permit = tokio::select! {
            result = self.sender.clone().reserve_owned() => match result {
                Ok(permit) => permit,
                Err(_) => return Err(mpsc::error::SendError(request)),
            },
            () = self.gate.wait_for_rejection() => {
                return Err(mpsc::error::SendError(request));
            }
        };
        if !self.gate.is_accepting() {
            return Err(mpsc::error::SendError(request));
        }
        permit.send(request);
        Ok(())
    }

    // Lifecycle control must still reach the actor after normal work is rejected.
    #[allow(clippy::result_large_err)]
    pub(super) async fn send_unchecked(
        &self,
        request: ProjectRequest,
    ) -> Result<(), mpsc::error::SendError<ProjectRequest>> {
        self.sender.send(request).await
    }
}

pub(super) enum ProjectRequest {
    Timed {
        request: Box<Self>,
        timing: ProjectRequestTiming,
    },
    Resident {
        request: Box<Self>,
        guard: residency::RustResidencyGuard,
    },
    Query {
        reply: oneshot::Sender<ProjectState>,
    },
    SetStatus {
        status: ProjectStatus,
        reply: oneshot::Sender<()>,
    },
    Refresh {
        reply: oneshot::Sender<ProjectState>,
    },
    Activate {
        root: PathBuf,
        reply: oneshot::Sender<Result<ProjectState, String>>,
    },
    ActivateWorkspaceRoots {
        roots: Vec<PathBuf>,
        reply: oneshot::Sender<Result<ProjectState, String>>,
    },
    Hover {
        file_path: String,
        line: u32,
        character: u32,
        reply: oneshot::Sender<Result<HoverResult, String>>,
    },
    Definition {
        file_path: String,
        line: u32,
        character: u32,
        page_token: Option<String>,
        reply: oneshot::Sender<Result<DefinitionResult, String>>,
    },
    References {
        file_path: String,
        line: u32,
        character: u32,
        include_declaration: bool,
        limits: SemanticResultLimits,
        page_offset: Option<usize>,
        reply: oneshot::Sender<Result<ReferencesResult, String>>,
    },
    ReadSourceResource {
        resource: SourceResource,
        max_response_bytes: usize,
        reply: oneshot::Sender<Result<SourceFrame, String>>,
    },
    ResolveSymbolHandle {
        symbol_handle: SymbolHandle,
        reply: oneshot::Sender<Result<ResolvedSymbolTarget, String>>,
    },
    Diagnostics {
        file_path: String,
        options: DiagnosticOptions,
        reply: oneshot::Sender<Result<DiagnosticsResult, String>>,
    },
    Rename {
        file_path: String,
        line: u32,
        character: u32,
        new_name: String,
        reply: oneshot::Sender<Result<RenameResult, String>>,
    },
    RenameWorkspaceEdit {
        file_path: String,
        line: u32,
        character: u32,
        new_name: String,
        reply: oneshot::Sender<Result<Option<WorkspaceEdit>, String>>,
    },
    Completions {
        file_path: String,
        line: u32,
        character: u32,
        trigger: Option<String>,
        page_token: Option<String>,
        reply: oneshot::Sender<Result<CompletionsResult, String>>,
    },
    DocumentSymbols {
        request: DocumentSymbolPageRequest,
        reply: oneshot::Sender<Result<DocumentSymbolsResult, String>>,
    },
    FormatDocument {
        file_path: String,
        tab_size: u32,
        insert_spaces: bool,
        reply: oneshot::Sender<Result<FormatDocumentResult, String>>,
    },
    FormatWorkspaceEdit {
        file_path: String,
        tab_size: u32,
        insert_spaces: bool,
        reply: oneshot::Sender<Result<Option<WorkspaceEdit>, String>>,
    },
    GeneratedEditPreview {
        project_id: String,
        request: GeneratedEditRequest,
        encoding: PositionEncoding,
        root: PathBuf,
        reply: oneshot::Sender<Result<GeneratedEditPreview, String>>,
    },
    SemanticDiscovery {
        file_path: String,
        line: u32,
        character: u32,
        kind: SemanticDiscoveryKind,
        page_token: Option<String>,
        reply: oneshot::Sender<Result<SemanticDiscoveryResult, String>>,
    },
    WorkspaceSymbol {
        request: WorkspaceSymbolPageRequest,
        reply: oneshot::Sender<Result<WorkspaceSymbolResult, String>>,
    },
    WorkspaceSymbolBatch {
        request: WorkspaceSymbolBatchRequest,
        reply: oneshot::Sender<Result<WorkspaceSymbolBatchResult, String>>,
    },
    LexicalSearch {
        request: LexicalSearchRequest,
        reply: oneshot::Sender<Result<LexicalSearchScan, String>>,
    },
    LexicalSearchBatch {
        request: LexicalSearchBatchRequest,
        reply: oneshot::Sender<Result<LexicalSearchBatchResult, String>>,
    },
    InspectSymbol {
        request: InspectSymbolRequest,
        reply: oneshot::Sender<Result<InspectSymbolResult, String>>,
    },
    InspectSymbolBatch {
        request: Box<InspectSymbolBatchRequest>,
        reply: oneshot::Sender<Result<InspectSymbolBatchResult, String>>,
    },
    CodeActions {
        file_path: String,
        start_line: u32,
        start_character: u32,
        end_line: u32,
        end_character: u32,
        kind_filter: Option<String>,
        page_token: Option<String>,
        reply: oneshot::Sender<Result<CodeActionsResult, String>>,
    },
    CodeActionList {
        file_path: String,
        start_line: u32,
        start_character: u32,
        end_line: u32,
        end_character: u32,
        kind_filter: Option<String>,
        page_token: Option<String>,
        reply: oneshot::Sender<Result<CodeActionsResult, String>>,
    },
    CodeActionPreview {
        action_id: PlanId,
        project_id: String,
        encoding: PositionEncoding,
        root: PathBuf,
        reply: oneshot::Sender<Result<PreviewArtifact, String>>,
    },
    PrepareCallHierarchy {
        file_path: String,
        line: u32,
        character: u32,
        page_token: Option<String>,
        reply: oneshot::Sender<Result<CallHierarchyPrepareResult, String>>,
    },
    IncomingCalls {
        item: serde_json::Value,
        limits: SemanticResultLimits,
        page_token: Option<String>,
        reply: oneshot::Sender<Result<IncomingCallsResult, String>>,
    },
    OutgoingCalls {
        item: serde_json::Value,
        limits: SemanticResultLimits,
        page_token: Option<String>,
        reply: oneshot::Sender<Result<OutgoingCallsResult, String>>,
    },
    SignatureHelp {
        file_path: String,
        line: u32,
        character: u32,
        page_token: Option<String>,
        reply: oneshot::Sender<Result<SignatureHelpResult, String>>,
    },
    InlayHints {
        file_path: String,
        start_line: u32,
        start_character: u32,
        end_line: u32,
        end_character: u32,
        page_token: Option<String>,
        reply: oneshot::Sender<Result<InlayHintsResult, String>>,
    },
    GoToImplementation {
        file_path: String,
        line: u32,
        character: u32,
        page_token: Option<String>,
        reply: oneshot::Sender<Result<LocationsResult, String>>,
    },
    GoToTypeDefinition {
        file_path: String,
        line: u32,
        character: u32,
        page_token: Option<String>,
        reply: oneshot::Sender<Result<LocationsResult, String>>,
    },
    CachedDiagnostics {
        file_path: String,
        options: DiagnosticOptions,
        reply: oneshot::Sender<Result<DiagnosticsResult, String>>,
    },
    HasCachedDiagnostics {
        file_path: String,
        reply: oneshot::Sender<Result<bool, String>>,
    },
    OpenDocumentPaths {
        reply: oneshot::Sender<Vec<PathBuf>>,
    },
    ValidatePath {
        file_path: String,
        reply: oneshot::Sender<Result<(), String>>,
    },
    SourcePathAuthorized {
        path: PathBuf,
        reply: oneshot::Sender<bool>,
    },
    AddWorkspaceRoot {
        root: PathBuf,
        reply: oneshot::Sender<Result<ProjectState, String>>,
    },
    StoreEditPlan {
        plan: EditPlan,
        reply: oneshot::Sender<Result<(), String>>,
    },
    TakeEditPlan {
        plan_id: PlanId,
        project_id: String,
        reply: oneshot::Sender<Result<EditPlan, String>>,
    },
    InspectEditPlan {
        plan_id: PlanId,
        project_id: String,
        reply: oneshot::Sender<Result<crate::edit_plan::EditPlanApprovalSummary, String>>,
    },
    ReadEditPlanDiff {
        plan_id: PlanId,
        project_id: String,
        reply: oneshot::Sender<Result<String, String>>,
    },
    ReadAppliedEditDetail {
        plan_id: PlanId,
        project_id: String,
        reply: oneshot::Sender<Result<String, String>>,
    },
    ApplyEditPlan {
        plan_id: PlanId,
        project_id: String,
        root: PathBuf,
        session_id: Option<String>,
        principal: Option<String>,
        lease: EditLease,
        reply: oneshot::Sender<Result<ApplyEditPlanOutcome, String>>,
    },
    FinalizeEditPlan {
        prepared: Box<PreparedEditPlan>,
        result: Result<ApplyReport, ApplyError>,
        reply: oneshot::Sender<Result<ApplyEditPlanOutcome, String>>,
    },
    PublishEvent {
        event: ProjectEvent,
        reply: oneshot::Sender<()>,
    },
    PreviewEdit {
        project_id: String,
        edit: WorkspaceEdit,
        encoding: PositionEncoding,
        root: PathBuf,
        reply: oneshot::Sender<Result<PreviewArtifact, String>>,
    },
    MoveInlineModulePreview {
        project_id: String,
        file_path: String,
        module_name: String,
        module_position: Option<lsp_types::Position>,
        encoding: PositionEncoding,
        root: PathBuf,
        reply: oneshot::Sender<Result<PreviewArtifact, String>>,
    },
    StructuralReplacePreview {
        project_id: String,
        request: StructuralReplaceRequest,
        root: PathBuf,
        reply: oneshot::Sender<Result<StructuralPreview, String>>,
    },
    PathRenamePreview {
        project_id: String,
        request: PathRenameRequest,
        root: PathBuf,
        reply: oneshot::Sender<Result<PathRenamePreview, String>>,
    },
    ServerLogs {
        limit: usize,
        min_level: Option<String>,
        cursor: Option<String>,
        reply: oneshot::Sender<Result<ServerLogsResult, String>>,
    },
    ServerMessages {
        limit: usize,
        cursor: Option<String>,
        reply: oneshot::Sender<Result<ServerMessagesResult, String>>,
    },
    ServerCapabilities {
        language_id: Option<String>,
        reply: oneshot::Sender<Result<Vec<ServerCapability>, String>>,
    },
    Notification {
        generation: u64,
        server_id: ServerId,
        notification: LspNotification,
    },
    ServerExited {
        generation: u64,
    },
    Restart {
        reply: oneshot::Sender<ProjectState>,
    },
    Fail {
        message: String,
        reply: oneshot::Sender<()>,
    },
    Shutdown {
        reply: oneshot::Sender<()>,
    },
    Suspend {
        reply: oneshot::Sender<Result<(), ()>>,
        dormancy: ProjectDormancy,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RustResidencyRequirement {
    None,
    Touch,
    Resume,
    Activate,
}

impl ProjectRequest {
    pub(super) fn into_resident(self) -> (Self, Option<residency::RustResidencyGuard>) {
        match self {
            Self::Resident { request, guard } => (*request, Some(guard)),
            request => (request, None),
        }
    }

    pub(super) fn into_timed(self) -> (Self, ProjectRequestTiming) {
        match self {
            Self::Timed { request, timing } => (*request, timing),
            request => (
                request,
                ProjectRequestTiming {
                    queued_at: Instant::now(),
                    span: tracing::Span::none(),
                },
            ),
        }
    }

    pub(super) const fn rust_residency_requirement(&self) -> RustResidencyRequirement {
        if let Self::Timed { request, .. } = self {
            return request.rust_residency_requirement();
        }
        if matches!(
            self,
            Self::Activate { .. }
                | Self::ActivateWorkspaceRoots { .. }
                | Self::AddWorkspaceRoot { .. }
                | Self::Restart { .. }
        ) {
            return RustResidencyRequirement::Activate;
        }
        if matches!(
            self,
            Self::Hover { .. }
                | Self::Definition { .. }
                | Self::References { .. }
                | Self::ResolveSymbolHandle { .. }
                | Self::Diagnostics { .. }
                | Self::Rename { .. }
                | Self::RenameWorkspaceEdit { .. }
                | Self::Completions { .. }
                | Self::DocumentSymbols { .. }
                | Self::FormatDocument { .. }
                | Self::FormatWorkspaceEdit { .. }
                | Self::SemanticDiscovery { .. }
                | Self::WorkspaceSymbol { .. }
                | Self::WorkspaceSymbolBatch { .. }
                | Self::InspectSymbol { .. }
                | Self::InspectSymbolBatch { .. }
                | Self::CodeActions { .. }
                | Self::CodeActionList { .. }
                | Self::PrepareCallHierarchy { .. }
                | Self::IncomingCalls { .. }
                | Self::OutgoingCalls { .. }
                | Self::SignatureHelp { .. }
                | Self::InlayHints { .. }
                | Self::GoToImplementation { .. }
                | Self::GoToTypeDefinition { .. }
                | Self::ApplyEditPlan { .. }
                | Self::MoveInlineModulePreview { .. }
                | Self::PathRenamePreview { .. }
                | Self::GeneratedEditPreview { .. }
        ) || matches!(
            self,
            Self::StructuralReplacePreview {
                request: StructuralReplaceRequest {
                    dialect: StructuralDialect::RustAnalyzerSsr,
                    ..
                },
                ..
            }
        ) {
            RustResidencyRequirement::Resume
        } else if matches!(
            self,
            Self::Resident { .. }
                | Self::SetStatus { .. }
                | Self::Suspend { .. }
                | Self::ServerExited { .. }
                | Self::Shutdown { .. }
                | Self::Fail { .. }
                | Self::FinalizeEditPlan { .. }
        ) {
            RustResidencyRequirement::None
        } else {
            RustResidencyRequirement::Touch
        }
    }

    pub(super) const fn rust_residency_mode(&self) -> Option<RustResidencyMode> {
        match self.rust_residency_requirement() {
            RustResidencyRequirement::None => None,
            RustResidencyRequirement::Touch => Some(RustResidencyMode::Touch),
            RustResidencyRequirement::Resume => Some(RustResidencyMode::Resume),
            RustResidencyRequirement::Activate => Some(RustResidencyMode::Activate),
        }
    }

    pub(super) const fn resumes_rust_runtime(&self) -> bool {
        matches!(
            self.rust_residency_requirement(),
            RustResidencyRequirement::Resume
        )
    }

    pub(super) const fn must_run_while_semantics_load(&self) -> bool {
        match self {
            Self::Timed { request, .. } | Self::Resident { request, .. } => {
                request.must_run_while_semantics_load()
            }
            Self::Activate { .. }
            | Self::ActivateWorkspaceRoots { .. }
            | Self::AddWorkspaceRoot { .. }
            | Self::Notification { .. }
            | Self::ServerExited { .. }
            | Self::SetStatus { .. }
            | Self::Restart { .. }
            | Self::Suspend { .. }
            | Self::Shutdown { .. }
            | Self::Fail { .. } => true,
            _ => false,
        }
    }

    /// Fail LSP work that was queued while this actor exhausted recovery.
    ///
    /// Inspection and lifecycle requests still pass through so callers can
    /// observe the failure and explicitly reactivate the project.
    pub(super) fn reject_if_failed(self, status: ProjectStatus) -> Result<Self, ()> {
        if status != ProjectStatus::Failed {
            return Ok(self);
        }

        macro_rules! reject {
            ($reply:expr) => {{
                let _ = $reply.send(Err(LANGUAGE_SERVER_EXITED.to_string()));
                return Err(());
            }};
        }

        match self {
            Self::Hover { reply, .. } => reject!(reply),
            Self::Definition { reply, .. } => reject!(reply),
            Self::References { reply, .. } => reject!(reply),
            Self::ResolveSymbolHandle { reply, .. } => reject!(reply),
            Self::Diagnostics { reply, .. } => reject!(reply),
            Self::Rename { reply, .. } => reject!(reply),
            Self::RenameWorkspaceEdit { reply, .. } | Self::FormatWorkspaceEdit { reply, .. } => {
                reject!(reply)
            }
            Self::GeneratedEditPreview { reply, .. } => reject!(reply),
            Self::SemanticDiscovery { reply, .. } => reject!(reply),
            Self::Completions { reply, .. } => reject!(reply),
            Self::DocumentSymbols { reply, .. } => reject!(reply),
            Self::FormatDocument { reply, .. } => reject!(reply),
            // Workspace-symbol lookup has an in-process AST fallback, so it
            // remains available even after all configured LSPs fail.
            Self::CodeActions { reply, .. } | Self::CodeActionList { reply, .. } => {
                reject!(reply)
            }
            Self::PrepareCallHierarchy { reply, .. } => reject!(reply),
            Self::IncomingCalls { reply, .. } => reject!(reply),
            Self::OutgoingCalls { reply, .. } => reject!(reply),
            Self::SignatureHelp { reply, .. } => reject!(reply),
            Self::InlayHints { reply, .. } => reject!(reply),
            Self::GoToImplementation { reply, .. } | Self::GoToTypeDefinition { reply, .. } => {
                reject!(reply)
            }
            request => Ok(request),
        }
    }
}

impl ProjectRequest {
    pub(super) fn is_cancelled(&self) -> bool {
        match self {
            Self::Timed { request, .. } | Self::Resident { request, .. } => request.is_cancelled(),
            Self::Query { reply } | Self::Refresh { reply } | Self::Restart { reply } => {
                reply.is_closed()
            }
            Self::SetStatus { reply, .. } | Self::Fail { reply, .. } => reply.is_closed(),
            Self::Activate { reply, .. } | Self::ActivateWorkspaceRoots { reply, .. } => {
                reply.is_closed()
            }
            Self::Hover { reply, .. } => reply.is_closed(),
            Self::Definition { reply, .. } => reply.is_closed(),
            Self::References { reply, .. } => reply.is_closed(),
            Self::ReadSourceResource { reply, .. } => reply.is_closed(),
            Self::ResolveSymbolHandle { reply, .. } => reply.is_closed(),
            Self::Diagnostics { reply, .. } | Self::CachedDiagnostics { reply, .. } => {
                reply.is_closed()
            }
            Self::Rename { reply, .. } => reply.is_closed(),
            Self::RenameWorkspaceEdit { reply, .. } | Self::FormatWorkspaceEdit { reply, .. } => {
                reply.is_closed()
            }
            Self::GeneratedEditPreview { reply, .. } => reply.is_closed(),
            Self::SemanticDiscovery { reply, .. } => reply.is_closed(),
            Self::Completions { reply, .. } => reply.is_closed(),
            Self::DocumentSymbols { reply, .. } => reply.is_closed(),
            Self::FormatDocument { reply, .. } => reply.is_closed(),
            Self::WorkspaceSymbol { reply, .. } => reply.is_closed(),
            Self::WorkspaceSymbolBatch { reply, .. } => reply.is_closed(),
            Self::LexicalSearch { reply, .. } => reply.is_closed(),
            Self::LexicalSearchBatch { reply, .. } => reply.is_closed(),
            Self::InspectSymbol { reply, .. } => reply.is_closed(),
            Self::InspectSymbolBatch { reply, .. } => reply.is_closed(),
            Self::CodeActions { reply, .. } | Self::CodeActionList { reply, .. } => {
                reply.is_closed()
            }
            Self::CodeActionPreview { reply, .. }
            | Self::PreviewEdit { reply, .. }
            | Self::MoveInlineModulePreview { reply, .. } => reply.is_closed(),
            Self::StructuralReplacePreview { reply, .. } => reply.is_closed(),
            Self::PathRenamePreview { reply, .. } => reply.is_closed(),
            Self::PrepareCallHierarchy { reply, .. } => reply.is_closed(),
            Self::IncomingCalls { reply, .. } => reply.is_closed(),
            Self::OutgoingCalls { reply, .. } => reply.is_closed(),
            Self::SignatureHelp { reply, .. } => reply.is_closed(),
            Self::InlayHints { reply, .. } => reply.is_closed(),
            Self::GoToImplementation { reply, .. } | Self::GoToTypeDefinition { reply, .. } => {
                reply.is_closed()
            }
            Self::HasCachedDiagnostics { reply, .. } => reply.is_closed(),
            Self::OpenDocumentPaths { reply } => reply.is_closed(),
            Self::ValidatePath { reply, .. } | Self::StoreEditPlan { reply, .. } => {
                reply.is_closed()
            }
            Self::SourcePathAuthorized { reply, .. } => reply.is_closed(),
            Self::AddWorkspaceRoot { reply, .. } => reply.is_closed(),
            Self::TakeEditPlan { reply, .. } => reply.is_closed(),
            Self::InspectEditPlan { reply, .. } => reply.is_closed(),
            Self::ReadEditPlanDiff { reply, .. } | Self::ReadAppliedEditDetail { reply, .. } => {
                reply.is_closed()
            }
            Self::ApplyEditPlan { reply, .. } => reply.is_closed(),
            Self::ServerLogs { reply, .. } => reply.is_closed(),
            Self::ServerMessages { reply, .. } => reply.is_closed(),
            Self::ServerCapabilities { reply, .. } => reply.is_closed(),
            Self::PublishEvent { .. }
            | Self::Shutdown { .. }
            | Self::Suspend { .. }
            | Self::Notification { .. }
            | Self::ServerExited { .. }
            | Self::FinalizeEditPlan { .. } => false,
        }
    }
}

/// Cloneable handle for querying and controlling one project actor.
#[derive(Clone)]
pub struct ProjectHandle {
    pub(super) sender: ProjectRequestSender,
    pub(super) status: watch::Receiver<ProjectStatus>,
    pub(super) state: watch::Receiver<ProjectState>,
    pub(super) events: broadcast::Sender<ProjectEvent>,
    pub(super) event_history: std::sync::Arc<std::sync::Mutex<ProjectEventHistory>>,
}

impl ProjectHandle {
    /// Return the actor request queue depth and fixed capacity without awaiting it.
    #[must_use]
    pub fn queue_pressure(&self) -> ProjectQueuePressure {
        self.sender.queue_pressure()
    }

    /// Subscribe to lifecycle changes for this project.
    #[must_use]
    pub fn status(&self) -> watch::Receiver<ProjectStatus> {
        self.status.clone()
    }
    /// Wait while an expected language server is completing its initial load.
    pub(crate) async fn wait_until_routable(&self) -> Result<(), ProjectActorError> {
        let mut state = self.state.clone();
        while state.borrow_and_update().runtime.semantic_readiness == SemanticReadiness::Loading {
            state
                .changed()
                .await
                .map_err(|_| ProjectActorError::Closed)?;
        }
        Ok(())
    }

    pub(super) fn state_snapshot(&self) -> ProjectState {
        self.state.borrow().clone()
    }

    /// Subscribe to typed project lifecycle and failure events.
    #[must_use]
    pub fn subscribe_events(&self) -> broadcast::Receiver<ProjectEvent> {
        self.events.subscribe()
    }

    /// Return retained project events newer than an optional polling cursor.
    #[must_use]
    pub fn event_snapshot(&self, cursor: Option<u64>, max_events: usize) -> ProjectEventSnapshot {
        self.event_history
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .snapshot_since(cursor, max_events)
    }

    /// Return one retained immutable event record by sequence.
    #[must_use]
    pub fn event_record(&self, sequence: u64) -> Option<ProjectEventRecord> {
        self.event_history
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .record_at(sequence)
    }

    pub(super) fn reject_new_work(&self) {
        self.sender.reject_new_work();
    }

    pub(super) fn accept_new_work(&self) {
        self.sender.accept_new_work();
    }

    pub(super) async fn publish_event(&self, event: ProjectEvent) -> Result<(), ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send_unchecked(ProjectRequest::PublishEvent { event, reply })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response.await.map_err(|_| ProjectActorError::Cancelled)
    }

    /// Query the actor's current state.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor has stopped or drops the response.
    pub async fn query(&self) -> Result<ProjectState, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::Query { reply })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response.await.map_err(|_| ProjectActorError::Cancelled)
    }

    /// Change the actor's observable lifecycle state.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor has stopped or drops the response.
    pub async fn set_status(&self, status: ProjectStatus) -> Result<(), ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::SetStatus { status, reply })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response.await.map_err(|_| ProjectActorError::Cancelled)
    }

    /// Refresh the actor's current state without mutating it.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor has stopped or drops the response.
    pub async fn refresh(&self) -> Result<ProjectState, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::Refresh { reply })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response.await.map_err(|_| ProjectActorError::Cancelled)
    }

    /// Activate the actor-owned language servers for its project root.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor is closed or language-server activation
    /// fails.
    pub async fn activate(&self, root: PathBuf) -> Result<ProjectState, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::Activate { root, reply })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    /// Activate the actor-owned language servers for all linked workspace roots.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor is closed or language-server activation
    /// fails.
    pub async fn activate_workspace_roots(
        &self,
        roots: Vec<PathBuf>,
    ) -> Result<ProjectState, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::ActivateWorkspaceRoots { roots, reply })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    /// Route a hover request through this project's actor-owned translator.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor is closed, cancels the response, or the
    /// actor-owned translator rejects the request.
    pub async fn hover(
        &self,
        file_path: String,
        line: u32,
        character: u32,
    ) -> Result<HoverResult, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::Hover {
                file_path,
                line,
                character,
                reply,
            })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    /// Route a definition request through this project's actor-owned translator.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor is closed, cancels the response, or the
    /// actor-owned translator rejects the request.
    pub async fn definition(
        &self,
        file_path: String,
        line: u32,
        character: u32,
    ) -> Result<DefinitionResult, ProjectActorError> {
        self.definition_page(file_path, line, character, None).await
    }

    /// Route one snapshot-bound definition page through the actor.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor is closed, cancels the response, or the
    /// actor-owned translator rejects the request.
    pub async fn definition_page(
        &self,
        file_path: String,
        line: u32,
        character: u32,
        page_token: Option<String>,
    ) -> Result<DefinitionResult, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::Definition {
                file_path,
                line,
                character,
                page_token,
                reply,
            })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    /// Route a references request through this project's actor-owned translator.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor is closed, cancels the response, or the
    /// actor-owned translator rejects the request.
    pub async fn references(
        &self,
        file_path: String,
        line: u32,
        character: u32,
        include_declaration: bool,
        limits: SemanticResultLimits,
    ) -> Result<ReferencesResult, ProjectActorError> {
        self.references_with_cursor(
            file_path,
            line,
            character,
            include_declaration,
            limits,
            None,
        )
        .await
    }

    /// Route one deterministic reference page through this project's actor.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor is closed, cancels the response, or the
    /// actor-owned translator rejects the request.
    pub async fn references_with_cursor(
        &self,
        file_path: String,
        line: u32,
        character: u32,
        include_declaration: bool,
        limits: SemanticResultLimits,
        page_offset: Option<usize>,
    ) -> Result<ReferencesResult, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::References {
                file_path,
                line,
                character,
                include_declaration,
                limits,
                page_offset,
                reply,
            })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    /// Read a snapshot-bound source context resource.
    pub(crate) async fn read_source_resource(
        &self,
        resource: SourceResource,
        max_response_bytes: usize,
    ) -> Result<SourceFrame, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::ReadSourceResource {
                resource,
                max_response_bytes,
                reply,
            })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    /// Resolve a snapshot-bound symbol handle and find its references.
    ///
    /// # Errors
    ///
    /// Returns a typed operation error when the handle is unknown, expired,
    /// belongs to another project actor, or its source snapshot is stale.
    pub(crate) async fn resolve_symbol_handle(
        &self,
        symbol_handle: SymbolHandle,
    ) -> Result<ResolvedSymbolTarget, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::ResolveSymbolHandle {
                symbol_handle,
                reply,
            })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    /// Route a diagnostics request through this project's actor-owned translator.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor is closed, cancels the response, or the
    /// actor-owned translator rejects the request.
    pub async fn diagnostics(
        &self,
        file_path: String,
    ) -> Result<DiagnosticsResult, ProjectActorError> {
        self.diagnostics_with_options(file_path, DiagnosticOptions::default())
            .await
    }

    /// Route a bounded, filtered diagnostics request through the project actor.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor closes, cancels, or rejects the request.
    pub async fn diagnostics_with_options(
        &self,
        file_path: String,
        options: DiagnosticOptions,
    ) -> Result<DiagnosticsResult, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::Diagnostics {
                file_path,
                options,
                reply,
            })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    /// Route a rename request through this project's actor-owned translator.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor is closed, cancels the response, or the
    /// actor-owned translator rejects the request.
    pub async fn rename(
        &self,
        file_path: String,
        line: u32,
        character: u32,
        new_name: String,
    ) -> Result<RenameResult, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::Rename {
                file_path,
                line,
                character,
                new_name,
                reply,
            })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    /// Request a raw LSP workspace edit for a rename.
    ///
    /// # Errors
    ///
    /// Returns an error when the actor is closed, the request is cancelled, or
    /// the actor-owned translator rejects the request.
    pub async fn rename_workspace_edit(
        &self,
        file_path: String,
        line: u32,
        character: u32,
        new_name: String,
    ) -> Result<Option<WorkspaceEdit>, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::RenameWorkspaceEdit {
                file_path,
                line,
                character,
                new_name,
                reply,
            })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    /// Route a completion request through this project's actor-owned translator.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor is closed, cancels the response, or the
    /// actor-owned translator rejects the request.
    pub async fn completions(
        &self,
        file_path: String,
        line: u32,
        character: u32,
        trigger: Option<String>,
        page_token: Option<String>,
    ) -> Result<CompletionsResult, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::Completions {
                file_path,
                line,
                character,
                trigger,
                page_token,
                reply,
            })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    /// Route a document-symbol request through this project's actor-owned translator.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor is closed, cancels the response, or the
    /// actor-owned translator rejects the request.
    pub async fn document_symbols(
        &self,
        file_path: String,
        options: DocumentSymbolOptions,
    ) -> Result<DocumentSymbolsResult, ProjectActorError> {
        self.document_symbol_page(DocumentSymbolPageRequest {
            file_path,
            options,
            max_bytes: 16 * 1024,
            page_token: None,
        })
        .await
    }

    /// Route one bounded document-symbol page through this project's actor.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor closes, the caller cancels, or outline
    /// generation or continuation validation fails.
    pub async fn document_symbol_page(
        &self,
        request: DocumentSymbolPageRequest,
    ) -> Result<DocumentSymbolsResult, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::DocumentSymbols { request, reply })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    /// Route a document-formatting request through this project's actor-owned translator.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor is closed, cancels the response, or the
    /// actor-owned translator rejects the request.
    pub async fn format_document(
        &self,
        file_path: String,
        tab_size: u32,
        insert_spaces: bool,
    ) -> Result<FormatDocumentResult, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::FormatDocument {
                file_path,
                tab_size,
                insert_spaces,
                reply,
            })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    /// Request a raw LSP workspace edit for document formatting.
    ///
    /// # Errors
    ///
    /// Returns an error when the actor is closed, the request is cancelled, or
    /// the actor-owned translator rejects the request.
    pub async fn format_workspace_edit(
        &self,
        file_path: String,
        tab_size: u32,
        insert_spaces: bool,
    ) -> Result<Option<WorkspaceEdit>, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::FormatWorkspaceEdit {
                file_path,
                tab_size,
                insert_spaces,
                reply,
            })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    /// Generate an LSP edit and snapshot its source in one actor request.
    pub(crate) async fn preview_generated_edit(
        &self,
        project_id: String,
        request: GeneratedEditRequest,
        encoding: PositionEncoding,
        root: PathBuf,
    ) -> Result<GeneratedEditPreview, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::GeneratedEditPreview {
                project_id,
                request,
                encoding,
                root,
                reply,
            })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    pub(crate) async fn semantic_discovery(
        &self,
        file_path: String,
        line: u32,
        character: u32,
        kind: SemanticDiscoveryKind,
        page_token: Option<String>,
    ) -> Result<SemanticDiscoveryResult, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::SemanticDiscovery {
                file_path,
                line,
                character,
                kind,
                page_token,
                reply,
            })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    /// Route a bounded workspace-symbol page through this project's actor-owned translator.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor is closed, cancels the response, or the
    /// actor-owned translator rejects the request.
    pub async fn workspace_symbol(
        &self,
        request: WorkspaceSymbolPageRequest,
    ) -> Result<WorkspaceSymbolResult, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::WorkspaceSymbol { request, reply })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    /// Route a bounded workspace-symbol batch through one project actor request.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor closes, the caller cancels, or any
    /// downstream workspace-symbol request fails.
    pub async fn workspace_symbol_batch(
        &self,
        request: WorkspaceSymbolBatchRequest,
    ) -> Result<WorkspaceSymbolBatchResult, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::WorkspaceSymbolBatch { request, reply })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    /// Search project snapshots with bounded lexical matching.
    pub(crate) async fn lexical_search(
        &self,
        request: LexicalSearchRequest,
    ) -> Result<LexicalSearchScan, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::LexicalSearch { request, reply })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    /// Search several lexical queries from one actor-owned source snapshot pass.
    pub(crate) async fn lexical_search_batch(
        &self,
        request: LexicalSearchBatchRequest,
    ) -> Result<LexicalSearchBatchResult, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::LexicalSearchBatch { request, reply })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    /// Resolve and inspect one symbol in a single actor-owned snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor closes, the caller cancels, or symbol
    /// resolution fails.
    pub async fn inspect_symbol(
        &self,
        request: InspectSymbolRequest,
    ) -> Result<InspectSymbolResult, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::InspectSymbol { request, reply })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    /// Inspect several symbols concurrently through one actor request.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor closes, the caller cancels, or symbol
    /// resolution fails.
    pub async fn inspect_symbol_batch(
        &self,
        request: InspectSymbolBatchRequest,
    ) -> Result<InspectSymbolBatchResult, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::InspectSymbolBatch {
                request: Box::new(request),
                reply,
            })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    /// Route a code-action request through this project's actor-owned translator.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor is closed, cancels the response, or the
    /// actor-owned translator rejects the request.
    #[allow(clippy::too_many_arguments)]
    pub async fn code_actions(
        &self,
        file_path: String,
        start_line: u32,
        start_character: u32,
        end_line: u32,
        end_character: u32,
        kind_filter: Option<String>,
        page_token: Option<String>,
    ) -> Result<CodeActionsResult, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::CodeActions {
                file_path,
                start_line,
                start_character,
                end_line,
                end_character,
                kind_filter,
                page_token,
                reply,
            })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    /// List code actions and retain bounded project-local references.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor is unavailable or the language server
    /// rejects the request.
    #[allow(clippy::too_many_arguments)]
    pub async fn code_action_list(
        &self,
        file_path: String,
        start_line: u32,
        start_character: u32,
        end_line: u32,
        end_character: u32,
        kind_filter: Option<String>,
        page_token: Option<String>,
    ) -> Result<CodeActionsResult, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::CodeActionList {
                file_path,
                start_line,
                start_character,
                end_line,
                end_character,
                kind_filter,
                page_token,
                reply,
            })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    /// Preview one retained code action reference.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor is unavailable, the action is stale, or
    /// its command/edit cannot be safely previewed.
    pub async fn preview_code_action(
        &self,
        action_id: PlanId,
        project_id: String,
        encoding: PositionEncoding,
        root: PathBuf,
    ) -> Result<PreviewArtifact, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::CodeActionPreview {
                action_id,
                project_id,
                encoding,
                root,
                reply,
            })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    /// Route call-hierarchy preparation through this project's actor-owned translator.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor is closed, cancels the response, or the
    /// actor-owned translator rejects the request.
    pub async fn prepare_call_hierarchy(
        &self,
        file_path: String,
        line: u32,
        character: u32,
        page_token: Option<String>,
    ) -> Result<CallHierarchyPrepareResult, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::PrepareCallHierarchy {
                file_path,
                line,
                character,
                page_token,
                reply,
            })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    /// Route incoming call hierarchy requests through this project's actor-owned translator.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor is closed, cancels the response, or the
    /// actor-owned translator rejects the request.
    pub async fn incoming_calls(
        &self,
        item: serde_json::Value,
        limits: SemanticResultLimits,
        page_token: Option<String>,
    ) -> Result<IncomingCallsResult, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::IncomingCalls {
                item,
                limits,
                page_token,
                reply,
            })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    /// Route outgoing call hierarchy requests through this project's actor-owned translator.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor is closed, cancels the response, or the
    /// actor-owned translator rejects the request.
    pub async fn outgoing_calls(
        &self,
        item: serde_json::Value,
        limits: SemanticResultLimits,
        page_token: Option<String>,
    ) -> Result<OutgoingCallsResult, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::OutgoingCalls {
                item,
                limits,
                page_token,
                reply,
            })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    /// Route signature help through this project's actor-owned translator.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor is closed, cancels the response, or the
    /// actor-owned translator rejects the request.
    pub async fn signature_help(
        &self,
        file_path: String,
        line: u32,
        character: u32,
        page_token: Option<String>,
    ) -> Result<SignatureHelpResult, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::SignatureHelp {
                file_path,
                line,
                character,
                page_token,
                reply,
            })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    /// Route inlay hints through this project's actor-owned translator.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor is closed, cancels the response, or the
    /// actor-owned translator rejects the request.
    pub async fn inlay_hints(
        &self,
        file_path: String,
        start_line: u32,
        start_character: u32,
        end_line: u32,
        end_character: u32,
        page_token: Option<String>,
    ) -> Result<InlayHintsResult, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::InlayHints {
                file_path,
                start_line,
                start_character,
                end_line,
                end_character,
                page_token,
                reply,
            })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    /// Route implementation lookup through this project's actor-owned translator.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor is closed, cancels the response, or the
    /// actor-owned translator rejects the request.
    pub async fn go_to_implementation(
        &self,
        file_path: String,
        line: u32,
        character: u32,
    ) -> Result<LocationsResult, ProjectActorError> {
        self.go_to_implementation_page(file_path, line, character, None)
            .await
    }

    /// Route one snapshot-bound implementation page through the actor.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor is closed, cancels the response, or the
    /// actor-owned translator rejects the request.
    pub async fn go_to_implementation_page(
        &self,
        file_path: String,
        line: u32,
        character: u32,
        page_token: Option<String>,
    ) -> Result<LocationsResult, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::GoToImplementation {
                file_path,
                line,
                character,
                page_token,
                reply,
            })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    /// Route type-definition lookup through this project's actor-owned translator.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor is closed, cancels the response, or the
    /// actor-owned translator rejects the request.
    pub async fn go_to_type_definition(
        &self,
        file_path: String,
        line: u32,
        character: u32,
    ) -> Result<LocationsResult, ProjectActorError> {
        self.go_to_type_definition_page(file_path, line, character, None)
            .await
    }

    /// Route one snapshot-bound type-definition page through the actor.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor is closed, cancels the response, or the
    /// actor-owned translator rejects the request.
    pub async fn go_to_type_definition_page(
        &self,
        file_path: String,
        line: u32,
        character: u32,
        page_token: Option<String>,
    ) -> Result<LocationsResult, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::GoToTypeDefinition {
                file_path,
                line,
                character,
                page_token,
                reply,
            })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    /// Route cached diagnostics through this project's actor-owned translator.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor is closed, cancels the response, or the
    /// actor-owned translator rejects the request.
    pub async fn cached_diagnostics(
        &self,
        file_path: String,
    ) -> Result<DiagnosticsResult, ProjectActorError> {
        self.cached_diagnostics_with_options(file_path, DiagnosticOptions::default())
            .await
    }

    /// Route bounded, filtered cached diagnostics through this project actor.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor closes, cancels, or rejects the request.
    pub async fn cached_diagnostics_with_options(
        &self,
        file_path: String,
        options: DiagnosticOptions,
    ) -> Result<DiagnosticsResult, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::CachedDiagnostics {
                file_path,
                options,
                reply,
            })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    /// Return whether cached diagnostics exist for a document path.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor is closed, cancels the response, or the
    /// actor-owned translator rejects the request.
    pub async fn has_cached_diagnostics(
        &self,
        file_path: String,
    ) -> Result<bool, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::HasCachedDiagnostics { file_path, reply })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    /// Return the paths of documents currently owned by this actor.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor has stopped before replying.
    pub async fn open_document_paths(&self) -> Result<Vec<PathBuf>, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::OpenDocumentPaths { reply })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response.await.map_err(|_| ProjectActorError::Cancelled)
    }

    /// Validate that a path belongs to this project's workspace.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor is closed, cancels the response, or the
    /// path is outside the actor-owned workspace roots.
    pub async fn validate_path(&self, file_path: String) -> Result<(), ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::ValidatePath { file_path, reply })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    /// Whether this actor has an active-LSP source-read capability for `path`.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor is closed or cancels the response.
    pub async fn source_path_is_authorized(
        &self,
        path: PathBuf,
    ) -> Result<bool, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::SourcePathAuthorized { path, reply })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response.await.map_err(|_| ProjectActorError::Cancelled)
    }

    /// Add a compatible linked-project root to this actor's workspace.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor is closed, cancels the response, or the
    /// language servers cannot be restarted with the expanded root set.
    pub async fn add_workspace_root(
        &self,
        root: PathBuf,
    ) -> Result<ProjectState, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::AddWorkspaceRoot { root, reply })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    /// Store a project-owned workspace edit preview for later application.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor is closed, cancels the response, or the
    /// bounded plan store rejects the plan.
    pub async fn store_edit_plan(&self, plan: EditPlan) -> Result<(), ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::StoreEditPlan { plan, reply })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    /// Preview and store one project-owned LSP workspace edit.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor is closed, the edit cannot be safely
    /// planned, or the bounded plan store rejects the resulting artifact.
    pub async fn preview_edit(
        &self,
        project_id: String,
        edit: WorkspaceEdit,
        encoding: PositionEncoding,
        root: PathBuf,
    ) -> Result<PreviewArtifact, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::PreviewEdit {
                project_id,
                edit,
                encoding,
                root,
                reply,
            })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    /// Build and store an inline Rust module move from actor-owned document state.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor is closed, the request is cancelled, or
    /// the project-owned refactor/preview rejects the operation.
    pub async fn move_inline_module_preview(
        &self,
        project_id: String,
        file_path: String,
        module_name: String,
        module_position: Option<lsp_types::Position>,
        encoding: PositionEncoding,
        root: PathBuf,
    ) -> Result<PreviewArtifact, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::MoveInlineModulePreview {
                project_id,
                file_path,
                module_name,
                module_position,
                encoding,
                root,
                reply,
            })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    /// Search or preview one explicitly selected structural replacement dialect.
    pub(crate) async fn structural_replace_preview(
        &self,
        project_id: String,
        request: StructuralReplaceRequest,
        root: PathBuf,
    ) -> Result<StructuralPreview, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::StructuralReplacePreview {
                project_id,
                request,
                root,
                reply,
            })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    pub(crate) async fn path_rename_preview(
        &self,
        project_id: String,
        request: PathRenameRequest,
        root: PathBuf,
    ) -> Result<PathRenamePreview, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::PathRenamePreview {
                project_id,
                request,
                root,
                reply,
            })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    /// Consume one project-owned workspace edit preview.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor is closed, cancels the response, or the
    /// plan is missing, expired, or owned by another project.
    pub async fn take_edit_plan(
        &self,
        plan_id: PlanId,
        project_id: String,
    ) -> Result<EditPlan, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::TakeEditPlan {
                plan_id,
                project_id,
                reply,
            })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    /// Inspect one project-owned edit plan without consuming it.
    pub(crate) async fn inspect_edit_plan(
        &self,
        plan_id: PlanId,
        project_id: String,
    ) -> Result<crate::edit_plan::EditPlanApprovalSummary, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::InspectEditPlan {
                plan_id,
                project_id,
                reply,
            })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    /// Read the complete immutable unified diff for one retained edit plan.
    pub(crate) async fn read_edit_plan_diff(
        &self,
        plan_id: PlanId,
        project_id: String,
    ) -> Result<String, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::ReadEditPlanDiff {
                plan_id,
                project_id,
                reply,
            })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    /// Read the complete immutable result for one committed edit plan.
    pub(crate) async fn read_applied_edit_detail(
        &self,
        plan_id: PlanId,
        project_id: String,
    ) -> Result<String, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::ReadAppliedEditDetail {
                plan_id,
                project_id,
                reply,
            })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    /// Apply a plan while holding the registry-owned path reservation.
    pub(crate) async fn apply_edit_plan_with_lease(
        &self,
        plan_id: PlanId,
        project_id: String,
        root: PathBuf,
        session_id: Option<String>,
        principal: Option<String>,
        lease: EditLease,
    ) -> Result<ApplyEditPlanOutcome, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::ApplyEditPlan {
                plan_id,
                project_id,
                root,
                session_id,
                principal,
                lease,
                reply,
            })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    /// Forward one language-server notification into this project's actor.
    #[cfg(test)]
    pub(crate) async fn notify(
        &self,
        generation: u64,
        server_id: ServerId,
        notification: LspNotification,
    ) -> Result<(), ProjectActorError> {
        self.sender
            .send(ProjectRequest::Notification {
                generation,
                server_id,
                notification,
            })
            .await
            .map_err(|_| ProjectActorError::Closed)
    }

    /// Return recent logs from this project's language servers.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor is closed, cancels the response, or the
    /// requested log filter is invalid.
    pub async fn server_logs(
        &self,
        limit: usize,
        min_level: Option<String>,
    ) -> Result<ServerLogsResult, ProjectActorError> {
        self.server_logs_page(limit, min_level, None).await
    }

    /// Return one snapshot-bound page of recent logs.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor is closed, cancels the response, or the
    /// requested log filter is invalid.
    pub async fn server_logs_page(
        &self,
        limit: usize,
        min_level: Option<String>,
        cursor: Option<String>,
    ) -> Result<ServerLogsResult, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::ServerLogs {
                limit,
                min_level,
                cursor,
                reply,
            })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    /// Return recent messages from this project's language servers.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor is closed or cancels the response.
    pub async fn server_messages(
        &self,
        limit: usize,
    ) -> Result<ServerMessagesResult, ProjectActorError> {
        self.server_messages_page(limit, None).await
    }

    /// Return one snapshot-bound page of recent messages.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor is closed or cancels the response.
    pub async fn server_messages_page(
        &self,
        limit: usize,
        cursor: Option<String>,
    ) -> Result<ServerMessagesResult, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::ServerMessages {
                limit,
                cursor,
                reply,
            })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    pub(super) async fn server_logs_unchecked(
        &self,
        limit: usize,
        min_level: Option<String>,
    ) -> Result<ServerLogsResult, ProjectActorError> {
        self.server_logs_page_unchecked(limit, min_level, None)
            .await
    }

    pub(super) async fn server_logs_page_unchecked(
        &self,
        limit: usize,
        min_level: Option<String>,
        cursor: Option<String>,
    ) -> Result<ServerLogsResult, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send_unchecked(ProjectRequest::ServerLogs {
                limit,
                min_level,
                cursor,
                reply,
            })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    pub(super) async fn server_messages_unchecked(
        &self,
        limit: usize,
    ) -> Result<ServerMessagesResult, ProjectActorError> {
        self.server_messages_page_unchecked(limit, None).await
    }

    pub(super) async fn server_messages_page_unchecked(
        &self,
        limit: usize,
        cursor: Option<String>,
    ) -> Result<ServerMessagesResult, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send_unchecked(ProjectRequest::ServerMessages {
                limit,
                cursor,
                reply,
            })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    pub(super) async fn server_capabilities_unchecked(
        &self,
        language_id: Option<String>,
    ) -> Result<Vec<ServerCapability>, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send_unchecked(ProjectRequest::ServerCapabilities { language_id, reply })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    /// Return negotiated capabilities for this project's active language servers.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor is closed or cancels the response.
    pub async fn server_capabilities(
        &self,
        language_id: Option<String>,
    ) -> Result<Vec<ServerCapability>, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::ServerCapabilities { language_id, reply })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    /// Restart the project actor's managed services.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor has stopped or drops the response.
    pub async fn restart(&self) -> Result<ProjectState, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::Restart { reply })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response.await.map_err(|_| ProjectActorError::Cancelled)
    }

    /// Record a failure and expose it through [`ProjectState::last_error`].
    ///
    /// # Errors
    ///
    /// Returns an error if the actor has stopped or drops the response.
    pub async fn fail(&self, message: impl Into<String>) -> Result<(), ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::Fail {
                message: message.into(),
                reply,
            })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response.await.map_err(|_| ProjectActorError::Cancelled)
    }

    /// Stop the actor after publishing `Stopped`.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor has already stopped or drops the response.
    pub async fn shutdown(&self) -> Result<(), ProjectActorError> {
        self.sender.begin_shutdown();
        let (reply, response) = oneshot::channel();
        self.sender
            .send_unchecked(ProjectRequest::Shutdown { reply })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response.await.map_err(|_| ProjectActorError::Cancelled)
    }
}

/// Spawn a bounded project actor with `Starting` as its initial status.
#[must_use]
pub fn spawn_project_actor(capacity: usize) -> ProjectHandle {
    spawn_project_actor_with_translator(capacity, Translator::new())
}

/// Spawn an actor whose translator is configured for one canonical project root.
#[must_use]
pub fn spawn_project_actor_for_root(capacity: usize, root: &CanonicalRoot) -> ProjectHandle {
    let mut translator = Translator::new();
    translator.set_workspace_roots(vec![root.as_path().to_path_buf()]);
    spawn_project_actor_with_translator(capacity, translator)
}

/// Spawn an actor using a configuration snapshot from the daemon translator.
#[must_use]
pub fn spawn_project_actor_for_root_with_template(
    capacity: usize,
    root: &CanonicalRoot,
    template: &TranslatorTemplate,
) -> ProjectHandle {
    spawn_project_actor_with_translator_and_safety(
        capacity,
        template.translator_for_root(root.as_path().to_path_buf()),
        template.edit_safety().cloned(),
    )
}

/// Spawn an actor with translator state owned exclusively by that actor.
#[must_use]
pub fn spawn_project_actor_with_translator(
    capacity: usize,
    translator: Translator,
) -> ProjectHandle {
    spawn_project_actor_with_translator_and_safety(capacity, translator, None)
}

pub(super) fn spawn_project_actor_with_translator_and_safety(
    capacity: usize,
    translator: Translator,
    edit_safety: Option<EditSafetyConfig>,
) -> ProjectHandle {
    spawn_project_actor_with_runtime(capacity, translator, edit_safety, None)
}

#[derive(Clone)]
pub(super) struct ProjectResidency {
    pub(super) controller: RustResidencyController,
    pub(super) group: RustGroupId,
}

impl ProjectResidency {
    pub(super) fn try_acquire_existing(&self) -> Option<residency::RustResidencyGuard> {
        self.controller.try_acquire_existing(self.group)
    }

    pub(super) fn try_acquire_existing_for_recovery(
        &self,
    ) -> Option<residency::RustResidencyGuard> {
        self.controller
            .try_acquire_existing_for_recovery(self.group)
    }

    pub(super) fn remove(&self) {
        self.controller.remove(self.group);
    }

    pub(super) async fn resident_request(
        &self,
        request: ProjectRequest,
        mode: RustResidencyMode,
    ) -> ProjectRequest {
        let guard = self.controller.acquire_for(self.group, mode).await;
        ProjectRequest::Resident {
            request: Box::new(request),
            guard,
        }
    }

    pub(super) fn touch_request(&self, request: ProjectRequest) -> ProjectRequest {
        let Some(guard) = self.try_acquire_existing() else {
            return request;
        };
        ProjectRequest::Resident {
            request: Box::new(request),
            guard,
        }
    }
}

pub(super) fn spawn_project_actor_with_runtime(
    capacity: usize,
    translator: Translator,
    edit_safety: Option<EditSafetyConfig>,
    residency: Option<ProjectResidency>,
) -> ProjectHandle {
    spawn_project_actor_with_deferred_results(
        capacity,
        translator,
        edit_safety,
        residency,
        std::sync::Arc::new(std::sync::Mutex::new(DeferredResultStore::new())),
    )
}

pub(super) fn spawn_project_actor_with_deferred_results(
    capacity: usize,
    translator: Translator,
    edit_safety: Option<EditSafetyConfig>,
    residency: Option<ProjectResidency>,
    deferred_results: std::sync::Arc<std::sync::Mutex<DeferredResultStore>>,
) -> ProjectHandle {
    spawn_project_actor_with_deferred_results_scoped(
        capacity,
        translator,
        edit_safety,
        residency,
        deferred_results,
        None,
    )
}

pub(super) fn spawn_project_actor_with_deferred_results_scoped(
    capacity: usize,
    translator: Translator,
    edit_safety: Option<EditSafetyConfig>,
    residency: Option<ProjectResidency>,
    deferred_results: std::sync::Arc<std::sync::Mutex<DeferredResultStore>>,
    deferred_scope: Option<String>,
) -> ProjectHandle {
    let (sender, receiver) = mpsc::channel(capacity.max(1));
    let actor_sender = sender.downgrade();
    let gate = ProjectRequestGate::new();
    if let Some(residency) = &residency {
        residency
            .controller
            .register(residency.group, actor_sender.clone());
    }
    let sender = residency.as_ref().map_or_else(
        || ProjectRequestSender::with_gate(sender.clone(), None, gate.clone()),
        |residency| {
            ProjectRequestSender::with_gate(sender.clone(), Some(residency.clone()), gate.clone())
        },
    );
    let runtime = ProjectRuntime::with_deferred_results_scoped(
        translator,
        edit_safety,
        deferred_results,
        deferred_scope,
    );
    let initial_state = ProjectState::new(ProjectStatus::Starting, runtime.summary());
    let (status_tx, status_rx) = watch::channel(ProjectStatus::Starting);
    let (state_tx, state_rx) = watch::channel(initial_state.clone());
    let (event_tx, _) = broadcast::channel(256);
    let event_sender = event_tx.clone();
    let event_history = std::sync::Arc::new(std::sync::Mutex::new(ProjectEventHistory::new(256)));
    let channels = ProjectActorChannels {
        status_tx,
        state_tx,
        event_tx,
        event_history: std::sync::Arc::clone(&event_history),
        gate,
    };
    tokio::spawn(run_project_actor(
        receiver,
        actor_sender,
        channels,
        initial_state,
        runtime,
        residency,
    ));
    ProjectHandle {
        sender,
        status: status_rx,
        state: state_rx,
        events: event_sender,
        event_history,
    }
}

pub(super) struct ProjectActorChannels {
    pub(super) status_tx: watch::Sender<ProjectStatus>,
    pub(super) state_tx: watch::Sender<ProjectState>,
    pub(super) event_tx: broadcast::Sender<ProjectEvent>,
    pub(super) event_history: std::sync::Arc<std::sync::Mutex<ProjectEventHistory>>,
    pub(super) gate: ProjectRequestGate,
}

impl ProjectActorChannels {
    pub(super) fn publish(&self, event: ProjectEvent) {
        self.event_history
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .record(event.clone());
        let _ = self.event_tx.send(event);
    }

    pub(super) fn publish_notification(
        &self,
        runtime: &mut ProjectRuntime,
        generation: u64,
        server_id: &ServerId,
        notification: LspNotification,
    ) {
        if !runtime.owns_generation(generation) {
            return;
        }
        if let Some(event) = runtime.notification(generation, server_id, notification) {
            self.publish(event);
        }
    }

    pub(super) fn publish_applied_edit(&self, applied: &AppliedEditPlan) {
        for event in applied.project_events() {
            self.publish(event);
        }
    }

    pub(super) fn publish_status(&self, state: &mut ProjectState, status: ProjectStatus) {
        state.status = status;
        if status == ProjectStatus::Dormant {
            state
                .dormancy
                .get_or_insert(ProjectDormancy::new(ProjectDormancyReason::Restored, None));
        } else {
            state.dormancy = None;
        }
        let _ = self.status_tx.send(status);
        self.publish_state(state);
        self.publish(ProjectEvent::StatusChanged {
            status,
            last_error: state.last_error.clone(),
        });
    }

    pub(super) fn publish_state(&self, state: &ProjectState) {
        self.state_tx.send_replace(state.clone());
    }

    pub(super) fn publish_failure(&self, state: &mut ProjectState, error: impl Into<String>) {
        state.last_error = Some(error.into());
        self.publish_status(state, ProjectStatus::Failed);
    }
}

#[allow(clippy::large_futures)]
pub(super) async fn run_project_actor(
    mut receiver: mpsc::Receiver<ProjectRequest>,
    actor_sender: mpsc::WeakSender<ProjectRequest>,
    channels: ProjectActorChannels,
    mut state: ProjectState,
    mut runtime: ProjectRuntime,
    residency: Option<ProjectResidency>,
) {
    let mut deferred_requests = VecDeque::new();
    loop {
        let request = if let Some(request) = deferred_requests.pop_front() {
            request
        } else {
            let Some(request) = next_project_request(&mut receiver).await else {
                break;
            };
            request
        };
        let (request, _residency_guard) = request.into_resident();
        let (request, timing) = request.into_timed();
        if matches!(&request, ProjectRequest::Shutdown { .. }) {
            while runtime.active_edit_workers > 0 {
                let Some(next) = next_project_request(&mut receiver).await else {
                    break;
                };
                let (next, _residency_guard) = next.into_resident();
                let (next, timing) = next.into_timed();
                let stop = handle_timed_project_request(
                    next,
                    timing,
                    &actor_sender,
                    &channels,
                    &mut state,
                    &mut runtime,
                    residency.as_ref(),
                )
                .await;
                state.sync_runtime(&runtime);
                channels.publish_state(&state);
                if stop {
                    break;
                }
            }
        }
        let resumes_runtime = request.resumes_rust_runtime();
        if residency.is_some()
            && resumes_runtime
            && !runtime.activation_is_reusable(state.status, runtime.translator.workspace_roots())
        {
            resume_project_runtime(&actor_sender, &channels, &mut state, &mut runtime).await;
        }
        while resumes_runtime && runtime.translator.is_initializing() {
            let Some(next) = next_project_request(&mut receiver).await else {
                break;
            };
            if !next.must_run_while_semantics_load() {
                deferred_requests.push_back(next);
                continue;
            }
            let (next, _residency_guard) = next.into_resident();
            let (next, timing) = next.into_timed();
            let stop = handle_timed_project_request(
                next,
                timing,
                &actor_sender,
                &channels,
                &mut state,
                &mut runtime,
                residency.as_ref(),
            )
            .await;
            state.sync_runtime(&runtime);
            channels.publish_state(&state);
            if stop {
                return;
            }
        }
        if request.is_cancelled() {
            continue;
        }
        let stop = handle_timed_project_request(
            request,
            timing,
            &actor_sender,
            &channels,
            &mut state,
            &mut runtime,
            residency.as_ref(),
        )
        .await;
        state.sync_runtime(&runtime);
        channels.publish_state(&state);
        if stop {
            break;
        }
    }
    if state.status != ProjectStatus::Stopped {
        stop_project_runtime(&channels, &mut state, &mut runtime, false).await;
    }
    if let Some(residency) = residency {
        residency.controller.remove(residency.group);
    }
}

#[allow(clippy::large_futures)]
pub(super) async fn handle_timed_project_request(
    request: ProjectRequest,
    timing: ProjectRequestTiming,
    actor_sender: &mpsc::WeakSender<ProjectRequest>,
    channels: &ProjectActorChannels,
    state: &mut ProjectState,
    runtime: &mut ProjectRuntime,
    residency: Option<&ProjectResidency>,
) -> bool {
    timing.span.record(
        "actor_queue_ms",
        u64::try_from(timing.queued_at.elapsed().as_millis()).unwrap_or(u64::MAX),
    );
    let started = Instant::now();
    let stop = handle_project_request(request, actor_sender, channels, state, runtime, residency)
        .instrument(timing.span.clone())
        .await;
    timing.span.record(
        "actor_execution_ms",
        u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
    );
    stop
}

pub(super) async fn resume_project_runtime(
    actor_sender: &mpsc::WeakSender<ProjectRequest>,
    channels: &ProjectActorChannels,
    state: &mut ProjectState,
    runtime: &mut ProjectRuntime,
) {
    let roots = runtime.translator.workspace_roots().to_vec();
    runtime.begin_activation();
    state.last_error = None;
    channels.publish_status(state, ProjectStatus::Starting);
    let cancellation = CancellationToken::new();
    match Box::pin(run_cancellable_transition(
        &channels.gate,
        cancellation.clone(),
        runtime.activate_workspace_roots(roots, cancellation),
    ))
    .await
    {
        Ok(activation) => {
            mark_project_started(activation, actor_sender, channels, state, runtime);
        }
        Err(error) => {
            state.sync_runtime(runtime);
            channels.publish_failure(state, error);
        }
    }
}

pub(super) async fn next_project_request(
    receiver: &mut mpsc::Receiver<ProjectRequest>,
) -> Option<ProjectRequest> {
    while let Some(request) = receiver.recv().await {
        if !request.is_cancelled() {
            return Some(request);
        }
    }
    None
}

pub(super) fn spawn_notification_forwarders(
    notification_receivers: Vec<(ServerId, mpsc::Receiver<LspNotification>)>,
    actor_sender: &mpsc::WeakSender<ProjectRequest>,
    gate: &ProjectRequestGate,
    generation: u64,
) {
    for (server_id, receiver) in notification_receivers {
        let sender = actor_sender.clone();
        let gate = gate.clone();
        tokio::spawn(forward_lsp_notifications(
            server_id, receiver, sender, gate, generation,
        ));
    }
}

pub(super) async fn forward_lsp_notifications(
    server_id: ServerId,
    mut receiver: mpsc::Receiver<LspNotification>,
    sender: mpsc::WeakSender<ProjectRequest>,
    gate: ProjectRequestGate,
    generation: u64,
) {
    while let Some(notification) = receiver.recv().await {
        if !gate.is_accepting() {
            break;
        }
        let Some(sender) = sender.upgrade() else {
            break;
        };
        if sender
            .send(ProjectRequest::Notification {
                generation,
                server_id: server_id.clone(),
                notification,
            })
            .await
            .is_err()
        {
            break;
        }
    }
    if gate.is_accepting()
        && let Some(sender) = sender.upgrade()
    {
        // Closing a receiver is also the normal result of intentional eviction.
        // Let the actor inspect its lifecycle state before acquiring residency.
        let _ = sender
            .send(ProjectRequest::ServerExited { generation })
            .await;
    }
}

pub(super) fn mark_project_started(
    activation: ProjectActivation,
    actor_sender: &mpsc::WeakSender<ProjectRequest>,
    channels: &ProjectActorChannels,
    state: &mut ProjectState,
    runtime: &mut ProjectRuntime,
) {
    runtime.reset_automatic_restart();
    runtime.record_activation(activation.health());
    spawn_notification_forwarders(
        activation.into_notification_receivers(),
        actor_sender,
        &channels.gate,
        runtime.generation(),
    );
    let status = runtime.readiness_status();
    if !runtime.translator.is_initializing() {
        let elapsed_ms = runtime.activation_started_at.take().map_or(0, |started| {
            u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
        });
        tracing::info!(
            stage = "readiness",
            ?status,
            stage_ms = elapsed_ms,
            "project activation readiness reached"
        );
    }
    publish_project_readiness(channels, state, runtime);
}

pub(super) const fn activation_status(
    health: ActivationHealth,
    initializing: bool,
) -> ProjectStatus {
    if initializing {
        ProjectStatus::Starting
    } else {
        match health {
            ActivationHealth::Ready => ProjectStatus::Ready,
            ActivationHealth::Degraded | ActivationHealth::StructuralOnly => {
                ProjectStatus::Degraded
            }
        }
    }
}

pub(super) fn publish_project_readiness(
    channels: &ProjectActorChannels,
    state: &mut ProjectState,
    runtime: &ProjectRuntime,
) {
    state.sync_runtime(runtime);
    channels.publish_status(state, runtime.readiness_status());
}

pub(super) async fn stop_project_runtime(
    channels: &ProjectActorChannels,
    state: &mut ProjectState,
    runtime: &mut ProjectRuntime,
    clear_error: bool,
) {
    runtime.begin_transition();
    if clear_error {
        state.last_error = None;
    }
    channels.publish_status(state, ProjectStatus::Stopping);
    if let Err(error) = runtime.shutdown().await {
        state.last_error = Some(error);
    }
    state.sync_runtime(runtime);
    channels.publish_status(state, ProjectStatus::Stopped);
}

pub(super) async fn suspend_project_runtime(
    channels: &ProjectActorChannels,
    state: &mut ProjectState,
    runtime: &mut ProjectRuntime,
    dormancy: ProjectDormancy,
) -> Result<(), ()> {
    if runtime.has_dirty_documents() {
        return Err(());
    }
    runtime.begin_transition();
    state.last_error = None;
    channels.publish_status(state, ProjectStatus::Stopping);
    if let Err(error) = runtime.shutdown().await {
        state.last_error = Some(error);
        state.sync_runtime(runtime);
        channels.publish_status(state, ProjectStatus::Failed);
        return Err(());
    }
    state.sync_runtime(runtime);
    state.dormancy = Some(dormancy);
    channels.publish_status(state, ProjectStatus::Dormant);
    Ok(())
}

pub(super) const PROJECT_SHUTDOWN_CANCELLED: &str = "project shutdown requested";
pub(super) const PROJECT_REQUEST_CANCELLED: &str = "project request cancelled";

pub(super) async fn run_cancellable_transition<T, F>(
    gate: &ProjectRequestGate,
    cancellation: CancellationToken,
    operation: F,
) -> Result<T, String>
where
    F: Future<Output = Result<T, String>>,
{
    tokio::pin!(operation);
    if !gate.is_accepting() {
        cancellation.cancel();
        let _ = operation.await;
        return Err(PROJECT_SHUTDOWN_CANCELLED.to_string());
    }

    tokio::select! {
        result = &mut operation => result,
        () = gate.wait_for_rejection() => {
            cancellation.cancel();
            let _ = operation.await;
            Err(PROJECT_SHUTDOWN_CANCELLED.to_string())
        }
    }
}

pub(super) async fn run_cancellable_transition_until_reply<T, F, R>(
    gate: &ProjectRequestGate,
    cancellation: CancellationToken,
    reply: &mut oneshot::Sender<R>,
    operation: F,
) -> Result<T, String>
where
    F: Future<Output = Result<T, String>>,
{
    tokio::pin!(operation);
    if !gate.is_accepting() {
        cancellation.cancel();
        let _ = operation.await;
        return Err(PROJECT_SHUTDOWN_CANCELLED.to_string());
    }

    tokio::select! {
        result = &mut operation => result,
        () = gate.wait_for_rejection() => {
            cancellation.cancel();
            let _ = operation.await;
            Err(PROJECT_SHUTDOWN_CANCELLED.to_string())
        }
        () = reply.closed() => {
            cancellation.cancel();
            let _ = operation.await;
            Err(PROJECT_REQUEST_CANCELLED.to_string())
        }
    }
}

// This exhaustive dispatcher keeps actor state transitions in one place; each
// request arm is intentionally small and independently typed.
#[allow(clippy::too_many_lines)]
#[allow(clippy::large_stack_frames)]
#[allow(clippy::large_futures)]
pub(super) async fn handle_project_request(
    request: ProjectRequest,
    actor_sender: &mpsc::WeakSender<ProjectRequest>,
    channels: &ProjectActorChannels,
    state: &mut ProjectState,
    runtime: &mut ProjectRuntime,
    residency: Option<&ProjectResidency>,
) -> bool {
    let Ok(request) = request.reject_if_failed(state.status) else {
        return false;
    };

    match request {
        ProjectRequest::Timed { .. } => {
            unreachable!("timed request must be unwrapped by the actor loop")
        }
        ProjectRequest::Resident { .. } => {
            unreachable!("resident request must be unwrapped by the actor loop")
        }
        ProjectRequest::Query { reply } | ProjectRequest::Refresh { reply } => {
            state.sync_runtime(runtime);
            let _ = reply.send(state.clone());
        }
        ProjectRequest::Suspend { reply, dormancy } => {
            let _ = reply.send(suspend_project_runtime(channels, state, runtime, dormancy).await);
        }
        ProjectRequest::Activate { root, mut reply } => {
            if runtime.activation_is_reusable(state.status, std::slice::from_ref(&root)) {
                state.sync_runtime(runtime);
                let _ = reply.send(Ok(state.clone()));
                return false;
            }
            runtime.begin_activation();
            state.last_error = None;
            channels.publish_status(state, ProjectStatus::Starting);
            let cancellation = CancellationToken::new();
            match run_cancellable_transition_until_reply(
                &channels.gate,
                cancellation.clone(),
                &mut reply,
                runtime.activate_workspace_roots(vec![root], cancellation),
            )
            .await
            {
                Ok(notification_receivers) => {
                    mark_project_started(
                        notification_receivers,
                        actor_sender,
                        channels,
                        state,
                        runtime,
                    );
                    let _ = reply.send(Ok(state.clone()));
                }
                Err(error) => {
                    state.sync_runtime(runtime);
                    if error != PROJECT_REQUEST_CANCELLED {
                        channels.publish_failure(state, error.clone());
                    }
                    if let Some(residency) = residency {
                        residency.remove();
                    }
                    let _ = reply.send(Err(error));
                }
            }
        }
        ProjectRequest::ActivateWorkspaceRoots { roots, mut reply } => {
            if runtime.activation_is_reusable(state.status, &roots) {
                state.sync_runtime(runtime);
                let _ = reply.send(Ok(state.clone()));
                return false;
            }
            runtime.begin_activation();
            state.last_error = None;
            channels.publish_status(state, ProjectStatus::Starting);
            let cancellation = CancellationToken::new();
            match run_cancellable_transition_until_reply(
                &channels.gate,
                cancellation.clone(),
                &mut reply,
                runtime.activate_workspace_roots(roots, cancellation),
            )
            .await
            {
                Ok(notification_receivers) => {
                    mark_project_started(
                        notification_receivers,
                        actor_sender,
                        channels,
                        state,
                        runtime,
                    );
                    let _ = reply.send(Ok(state.clone()));
                }
                Err(error) => {
                    state.sync_runtime(runtime);
                    if error != PROJECT_REQUEST_CANCELLED {
                        channels.publish_failure(state, error.clone());
                    }
                    if let Some(residency) = residency {
                        residency.remove();
                    }
                    let _ = reply.send(Err(error));
                }
            }
        }
        ProjectRequest::Hover {
            file_path,
            line,
            character,
            reply,
        } => {
            let _ = reply.send(runtime.hover(file_path, line, character).await);
        }
        ProjectRequest::Definition {
            file_path,
            line,
            character,
            page_token,
            reply,
        } => {
            let _ = reply.send(
                runtime
                    .definition(file_path, line, character, page_token)
                    .await,
            );
        }
        ProjectRequest::References {
            file_path,
            line,
            character,
            include_declaration,
            limits,
            page_offset,
            reply,
        } => {
            let _ = reply.send(
                runtime
                    .references(
                        file_path,
                        line,
                        character,
                        include_declaration,
                        limits,
                        page_offset,
                    )
                    .await,
            );
        }
        ProjectRequest::ReadSourceResource {
            resource,
            max_response_bytes,
            reply,
        } => {
            let _ = reply.send(
                runtime
                    .read_source_resource(resource, max_response_bytes)
                    .await,
            );
        }
        ProjectRequest::ResolveSymbolHandle {
            symbol_handle,
            reply,
        } => {
            let _ = reply.send(runtime.resolve_symbol_handle(symbol_handle).await);
        }
        ProjectRequest::Diagnostics {
            file_path,
            options,
            reply,
        } => {
            let _ = reply.send(runtime.diagnostics(file_path, options).await);
        }
        ProjectRequest::Rename {
            file_path,
            line,
            character,
            new_name,
            reply,
        } => {
            let _ = reply.send(runtime.rename(file_path, line, character, new_name).await);
        }
        ProjectRequest::RenameWorkspaceEdit {
            file_path,
            line,
            character,
            new_name,
            reply,
        } => {
            let _ = reply.send(
                runtime
                    .rename_workspace_edit(file_path, line, character, new_name)
                    .await,
            );
        }
        ProjectRequest::Completions {
            file_path,
            line,
            character,
            trigger,
            page_token,
            reply,
        } => {
            let _ = reply.send(
                runtime
                    .completions(file_path, line, character, trigger, page_token)
                    .await,
            );
        }
        ProjectRequest::DocumentSymbols { request, reply } => {
            let _ = reply.send(runtime.document_symbols(request).await);
        }
        ProjectRequest::FormatDocument {
            file_path,
            tab_size,
            insert_spaces,
            reply,
        } => {
            let _ = reply.send(
                runtime
                    .format_document(file_path, tab_size, insert_spaces)
                    .await,
            );
        }
        ProjectRequest::FormatWorkspaceEdit {
            file_path,
            tab_size,
            insert_spaces,
            reply,
        } => {
            let _ = reply.send(
                runtime
                    .format_workspace_edit(file_path, tab_size, insert_spaces)
                    .await,
            );
        }
        ProjectRequest::GeneratedEditPreview {
            project_id,
            request,
            encoding,
            root,
            reply,
        } => {
            let _ = reply.send(
                runtime
                    .preview_generated_edit(&project_id, request, encoding, &root)
                    .await,
            );
        }
        ProjectRequest::SemanticDiscovery {
            file_path,
            line,
            character,
            kind,
            page_token,
            reply,
        } => {
            let _ = reply.send(
                runtime
                    .semantic_discovery(file_path, line, character, kind, page_token)
                    .await,
            );
        }
        ProjectRequest::WorkspaceSymbol { request, mut reply } => {
            if reply.is_closed() {
                return false;
            }
            let result = tokio::select! {
                () = reply.closed() => return false,
                result = runtime.workspace_symbol_page(request) => result,
            };
            let _ = reply.send(result);
        }
        ProjectRequest::WorkspaceSymbolBatch { request, mut reply } => {
            if reply.is_closed() {
                return false;
            }
            let result = tokio::select! {
                () = reply.closed() => return false,
                result = runtime.workspace_symbol_batch(request) => result,
            };
            let _ = reply.send(result);
        }
        ProjectRequest::LexicalSearch { request, mut reply } => {
            if reply.is_closed() {
                return false;
            }
            let result = tokio::select! {
                () = reply.closed() => return false,
                result = runtime.lexical_search(request) => result,
            };
            let _ = reply.send(result);
        }
        ProjectRequest::LexicalSearchBatch { request, mut reply } => {
            if reply.is_closed() {
                return false;
            }
            let result = tokio::select! {
                () = reply.closed() => return false,
                result = runtime.lexical_search_batch(request) => result,
            };
            let _ = reply.send(result);
        }
        ProjectRequest::InspectSymbol { request, mut reply } => {
            if reply.is_closed() {
                return false;
            }
            let result = tokio::select! {
                () = reply.closed() => return false,
                result = runtime.inspect_symbol(request) => result,
            };
            let _ = reply.send(result);
        }
        ProjectRequest::InspectSymbolBatch { request, mut reply } => {
            if reply.is_closed() {
                return false;
            }
            let result = tokio::select! {
                () = reply.closed() => return false,
                result = runtime.inspect_symbol_batch(*request) => result,
            };
            let _ = reply.send(result);
        }
        ProjectRequest::CodeActions {
            file_path,
            start_line,
            start_character,
            end_line,
            end_character,
            kind_filter,
            page_token,
            reply,
        } => {
            let _ = reply.send(
                runtime
                    .code_actions(
                        file_path,
                        start_line,
                        start_character,
                        end_line,
                        end_character,
                        kind_filter,
                        page_token,
                    )
                    .await,
            );
        }
        ProjectRequest::CodeActionList {
            file_path,
            start_line,
            start_character,
            end_line,
            end_character,
            kind_filter,
            page_token,
            reply,
        } => {
            let _ = reply.send(
                runtime
                    .code_action_list(
                        file_path,
                        start_line,
                        start_character,
                        end_line,
                        end_character,
                        kind_filter,
                        page_token,
                    )
                    .await,
            );
        }
        ProjectRequest::CodeActionPreview {
            action_id,
            project_id,
            encoding,
            root,
            reply,
        } => {
            let _ = reply.send(
                runtime
                    .preview_code_action(action_id, &project_id, encoding, &root)
                    .await,
            );
        }
        ProjectRequest::PrepareCallHierarchy {
            file_path,
            line,
            character,
            page_token,
            reply,
        } => {
            let _ = reply.send(
                runtime
                    .prepare_call_hierarchy(file_path, line, character, page_token)
                    .await,
            );
        }
        ProjectRequest::IncomingCalls {
            item,
            limits,
            page_token,
            reply,
        } => {
            let _ = reply.send(runtime.incoming_calls(item, limits, page_token).await);
        }
        ProjectRequest::OutgoingCalls {
            item,
            limits,
            page_token,
            reply,
        } => {
            let _ = reply.send(runtime.outgoing_calls(item, limits, page_token).await);
        }
        ProjectRequest::SignatureHelp {
            file_path,
            line,
            character,
            page_token,
            reply,
        } => {
            let _ = reply.send(
                runtime
                    .signature_help(file_path, line, character, page_token)
                    .await,
            );
        }
        ProjectRequest::InlayHints {
            file_path,
            start_line,
            start_character,
            end_line,
            end_character,
            page_token,
            reply,
        } => {
            let _ = reply.send(
                runtime
                    .inlay_hints(
                        file_path,
                        start_line,
                        start_character,
                        end_line,
                        end_character,
                        page_token,
                    )
                    .await,
            );
        }
        ProjectRequest::GoToImplementation {
            file_path,
            line,
            character,
            page_token,
            reply,
        } => {
            let _ = reply.send(
                runtime
                    .go_to_implementation(file_path, line, character, page_token)
                    .await,
            );
        }
        ProjectRequest::GoToTypeDefinition {
            file_path,
            line,
            character,
            page_token,
            reply,
        } => {
            let _ = reply.send(
                runtime
                    .go_to_type_definition(file_path, line, character, page_token)
                    .await,
            );
        }
        ProjectRequest::CachedDiagnostics {
            file_path,
            options,
            reply,
        } => {
            let _ = reply.send(runtime.cached_diagnostics(&file_path, options).await);
        }
        ProjectRequest::HasCachedDiagnostics { file_path, reply } => {
            let _ = reply.send(runtime.has_cached_diagnostics(&file_path));
        }
        ProjectRequest::OpenDocumentPaths { reply } => {
            let _ = reply.send(runtime.open_document_paths());
        }
        ProjectRequest::ValidatePath { file_path, reply } => {
            let _ = reply.send(runtime.validate_path(&file_path));
        }
        ProjectRequest::SourcePathAuthorized { path, reply } => {
            let _ = reply.send(runtime.source_path_is_authorized(&path));
        }
        ProjectRequest::AddWorkspaceRoot { root, reply } => {
            let previous_status = state.status;
            runtime.begin_activation();
            state.last_error = None;
            channels.publish_status(state, ProjectStatus::Restarting);
            let cancellation = CancellationToken::new();
            match run_cancellable_transition(
                &channels.gate,
                cancellation.clone(),
                runtime.add_workspace_root(root, previous_status, cancellation),
            )
            .await
            {
                Ok(notification_receivers) => {
                    mark_project_started(
                        notification_receivers,
                        actor_sender,
                        channels,
                        state,
                        runtime,
                    );
                    let _ = reply.send(Ok(state.clone()));
                }
                Err(error) => {
                    state.sync_runtime(runtime);
                    channels.publish_failure(state, error.clone());
                    let _ = reply.send(Err(error));
                }
            }
        }
        ProjectRequest::StoreEditPlan { plan, reply } => {
            let _ = reply.send(runtime.store_edit_plan(plan));
        }
        ProjectRequest::PreviewEdit {
            project_id,
            edit,
            encoding,
            root,
            reply,
        } => {
            let _ = reply.send(
                runtime
                    .preview_edit(&project_id, edit, encoding, &root)
                    .await,
            );
        }
        ProjectRequest::MoveInlineModulePreview {
            project_id,
            file_path,
            module_name,
            module_position,
            encoding,
            root,
            reply,
        } => {
            let _ = reply.send(
                runtime
                    .move_inline_module_preview(
                        &project_id,
                        &file_path,
                        &module_name,
                        module_position,
                        encoding,
                        &root,
                    )
                    .await,
            );
        }
        ProjectRequest::StructuralReplacePreview {
            project_id,
            request,
            root,
            mut reply,
        } => {
            let operation = runtime.structural_replace_preview(&project_id, request, &root);
            tokio::pin!(operation);
            tokio::select! {
                result = &mut operation => {
                    let _ = reply.send(result);
                }
                () = reply.closed() => {}
            }
        }
        ProjectRequest::PathRenamePreview {
            project_id,
            request,
            root,
            mut reply,
        } => {
            let operation = runtime.path_rename_preview(&project_id, request, &root);
            tokio::pin!(operation);
            tokio::select! {
                result = &mut operation => {
                    let _ = reply.send(result);
                }
                () = reply.closed() => {}
            }
        }
        ProjectRequest::TakeEditPlan {
            plan_id,
            project_id,
            reply,
        } => {
            let _ = reply.send(runtime.take_edit_plan(&plan_id, &project_id));
        }
        ProjectRequest::InspectEditPlan {
            plan_id,
            project_id,
            reply,
        } => {
            let _ = reply.send(runtime.inspect_edit_plan(&plan_id, &project_id));
        }
        ProjectRequest::ReadEditPlanDiff {
            plan_id,
            project_id,
            reply,
        } => {
            let _ = reply.send(runtime.read_edit_plan_diff(&plan_id, &project_id));
        }
        ProjectRequest::ReadAppliedEditDetail {
            plan_id,
            project_id,
            reply,
        } => {
            let _ = reply.send(runtime.read_applied_edit_detail(&plan_id, &project_id));
        }
        ProjectRequest::ApplyEditPlan {
            plan_id,
            project_id,
            root,
            session_id,
            principal,
            lease,
            reply,
        } => {
            match runtime.prepare_edit_plan_with_context(
                &plan_id,
                &project_id,
                &root,
                session_id,
                principal,
                lease,
            ) {
                Ok(PreparedEditResult::AlreadyApplied(applied)) => {
                    let _ = reply.send(Ok(ApplyEditPlanOutcome::Applied(applied)));
                }
                Ok(PreparedEditResult::AlreadyConflicted(conflict)) => {
                    let _ = reply.send(Ok(ApplyEditPlanOutcome::Conflict(conflict)));
                }
                Ok(PreparedEditResult::Ready(prepared)) => {
                    runtime.active_edit_workers = runtime.active_edit_workers.saturating_add(1);
                    let sender = actor_sender.clone();
                    tokio::spawn(async move {
                        let worker = tokio::task::spawn_blocking(move || {
                            let apply_result = prepared.backup_policy.as_ref().map_or_else(
                                || {
                                    apply_plan_with_documents(
                                        &prepared.boundary,
                                        &prepared.plan,
                                        &prepared.documents,
                                    )
                                },
                                |policy| {
                                    apply_plan_with_documents_and_backup(
                                        &prepared.boundary,
                                        &prepared.plan,
                                        &prepared.documents,
                                        policy,
                                    )
                                },
                            );
                            (prepared, apply_result)
                        })
                        .await;
                        match worker {
                            Ok((prepared, result)) => {
                                let Some(sender) = sender.upgrade() else {
                                    return;
                                };
                                let _ = sender
                                    .send(ProjectRequest::FinalizeEditPlan {
                                        prepared,
                                        result,
                                        reply,
                                    })
                                    .await;
                            }
                            Err(error) => {
                                let _ =
                                    reply.send(Err(format!("edit commit worker failed: {error}")));
                            }
                        }
                    });
                }
                Err(error) => {
                    let _ = reply.send(Err(error));
                }
            }
        }
        ProjectRequest::FinalizeEditPlan {
            prepared,
            result,
            reply,
        } => {
            runtime.active_edit_workers = runtime.active_edit_workers.saturating_sub(1);
            let result = runtime.finish_prepared_edit(*prepared, result).await;
            if let Ok(ApplyEditPlanOutcome::Applied(applied)) = &result {
                channels.publish_applied_edit(applied);
            }
            let _ = reply.send(result);
        }
        ProjectRequest::PublishEvent { event, reply } => {
            channels.publish(event);
            let _ = reply.send(());
        }
        ProjectRequest::ServerLogs {
            limit,
            min_level,
            cursor,
            reply,
        } => {
            let _ = reply.send(runtime.server_logs_page(limit, min_level, cursor.as_deref()));
        }
        ProjectRequest::ServerMessages {
            limit,
            cursor,
            reply,
        } => {
            let _ = reply.send(runtime.server_messages_page(limit, cursor.as_deref()));
        }
        ProjectRequest::ServerCapabilities { language_id, reply } => {
            let _ = reply.send(runtime.server_capabilities(language_id.as_deref()));
        }
        ProjectRequest::Notification {
            generation,
            server_id,
            notification,
        } => {
            let was_initializing = runtime.translator.is_initializing();
            channels.publish_notification(runtime, generation, &server_id, notification);
            if was_initializing && !runtime.translator.is_initializing() {
                let status = runtime.readiness_status();
                let elapsed_ms = runtime.activation_started_at.take().map_or(0, |started| {
                    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
                });
                tracing::info!(
                    stage = "readiness",
                    ?status,
                    stage_ms = elapsed_ms,
                    "project activation readiness reached"
                );
                publish_project_readiness(channels, state, runtime);
            }
        }
        ProjectRequest::ServerExited { generation } => {
            handle_server_exit(
                generation,
                actor_sender,
                channels,
                state,
                runtime,
                residency,
            )
            .await;
        }
        ProjectRequest::SetStatus { status, reply } => {
            state.sync_runtime(runtime);
            state.last_error = None;
            channels.publish_status(state, status);
            let _ = reply.send(());
        }
        ProjectRequest::Restart { reply } => {
            runtime.begin_activation();
            state.sync_runtime(runtime);
            state.last_error = None;
            channels.publish_status(state, ProjectStatus::Restarting);
            let cancellation = CancellationToken::new();
            match run_cancellable_transition(
                &channels.gate,
                cancellation.clone(),
                runtime.restart(cancellation),
            )
            .await
            {
                Ok(notification_receivers) => {
                    mark_project_started(
                        notification_receivers,
                        actor_sender,
                        channels,
                        state,
                        runtime,
                    );
                    let _ = reply.send(state.clone());
                }
                Err(error) => {
                    state.sync_runtime(runtime);
                    channels.publish_failure(state, error);
                    let _ = reply.send(state.clone());
                }
            }
        }
        ProjectRequest::Fail { message, reply } => {
            state.sync_runtime(runtime);
            channels.publish_failure(state, message);
            let _ = reply.send(());
        }
        ProjectRequest::Shutdown { reply } => {
            stop_project_runtime(channels, state, runtime, true).await;
            let _ = reply.send(());
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::ProjectRequestGate;

    #[test]
    fn request_gate_closes_and_reopens_admission() {
        let gate = ProjectRequestGate::new();
        assert!(gate.is_accepting());

        gate.reject_new_work();
        assert!(!gate.is_accepting());

        gate.accept_new_work();
        assert!(gate.is_accepting());
    }
}
