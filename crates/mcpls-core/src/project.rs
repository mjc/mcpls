//! Project identity, actor, runtime, and registry façade.

mod actor;
mod identity;
mod registry;
mod residency;
mod runtime;
mod state;
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::wildcard_imports)]
mod tests;
use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use ignore::WalkBuilder;
const CALL_HIERARCHY_PAGE_SIZE: usize = 64;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex, Notify, RwLock, broadcast, mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;
use tracing::Instrument as _;

use crate::bridge::DeferredResourceReference;
use crate::bridge::ast_grep::byte_offset_to_position;
use crate::bridge::convert_code_action_or_command;
use crate::bridge::lexical::{
    LexicalSearchBatchRequest, LexicalSearchBatchResult, LexicalSearchMatch, LexicalSearchRequest,
    LexicalSearchScan, collect_project_paths_filtered, find_matches,
};
use crate::bridge::resources::SourceResource;
use crate::bridge::resources::make_source_uri;
use crate::bridge::translator::SourceBudget;
use crate::bridge::translator::{
    CallHierarchyItemResult, DiagnosticOptions, SourceUnavailableReason, WorkspaceSymbol,
    page_items,
};
use crate::bridge::{
    ActivationHealth, CallHierarchyPrepareResult, CodeActionsResult, CompletionsResult,
    DefinitionResult, DiagnosticSeverity, DiagnosticsResult, DocumentSymbolOptions,
    DocumentSymbolPageRequest, DocumentSymbolsResult, FormatDocumentResult, HoverResult,
    IncomingCallsResult, InlayHintsResult, InspectSymbolBatchEntry, InspectSymbolBatchRequest,
    InspectSymbolBatchResult, InspectSymbolRequest, InspectSymbolResult, LocationsResult, LogEntry,
    LogLevel, OutgoingCallsResult, PositionEncoding, ProjectActivation, ProviderSynchronization,
    ReferencesResult, RenameResult, SemanticDiscoveryKind, SemanticDiscoveryResult,
    SemanticResultLimits, ServerCapability, ServerLogsResult, ServerMessage, ServerMessagesResult,
    SignatureHelpResult, SourceContext, SourceFrame, StructuralFileSnapshot, StructuralMatch,
    StructuralSearchResult, SymbolHandle, Translator, TranslatorTemplate, WillRenameFilesResult,
    WorkspaceSymbolBatchEntry, WorkspaceSymbolBatchRequest, WorkspaceSymbolBatchResult,
    WorkspaceSymbolMatchMode, WorkspaceSymbolPageRequest, WorkspaceSymbolResult,
    WorkspaceSymbolScope, path_to_uri, uri_to_path,
};
use crate::config::{EditSafetyConfig, ProjectConfig, ServerId};
use crate::edit_apply::{
    ApplyError, ApplyReport, apply_plan_with_documents, apply_plan_with_documents_and_backup,
};
use crate::edit_backup::BackupPolicy;
use crate::edit_coordinator::{EditCoordinator, EditLease};
use crate::edit_paths::{FileOperation, OperationValidationError, WorkspaceBoundary};
use crate::edit_plan::{AuditLogPolicy, EditAuditRecord, EditPlan, EditPlanStore, PlanId};
use crate::edit_preview::{
    EditProducer, PreviewArtifact, PreviewLimits, VerificationStatus, preview_workspace_edit,
    refresh_workspace_edit_documents,
};
use crate::lsp::{LspNotification, load_project_environment, resolve_command};
use crate::project_persistence::{PersistedProject, ProjectRegistrationStore};
use crate::rust_refactor::{logical_module_name, move_inline_module_preview_with_source};
use crate::workspace_edit::{EditOperation, normalize};
use lsp_types::WorkspaceEdit;
use residency::{RustGroupId, RustResidencyController, RustResidencyMode};

pub use actor::{
    ProjectActorError, ProjectHandle, spawn_project_actor, spawn_project_actor_for_root,
    spawn_project_actor_for_root_with_template, spawn_project_actor_with_translator,
};
pub use identity::{
    CanonicalRoot, GitRepositoryIdentity, GitRepositoryIdentityError, ProjectId, ProjectIdentity,
    ProjectIdentityError, ProjectResolver, longest_matching_root,
};
pub use registry::{
    ProjectQueuePressure, ProjectRegistry, ProjectRegistryError, ProjectRegistryStatusSnapshot,
    ProjectServerCapability, ProjectShutdownFailure, ProjectShutdownReport, ProjectStatusCounts,
    ProjectStatusSummary,
};
pub(crate) use runtime::lexical_page_cursor;
pub use runtime::{AppliedEditPlan, ApplyEditPlanOutcome, EditConflict, EditNotReady};
#[allow(unused_imports)]
pub(crate) use runtime::{
    DeferredResourcePayload, GeneratedEditPreview, GeneratedEditRequest, PathRenamePreview,
    PathRenameRequest, ResolvedSymbolTarget, StructuralDialect, StructuralMatchedFile,
    StructuralPreview, StructuralReplaceRequest,
};
pub use state::{
    ProjectDormancy, ProjectDormancyReason, ProjectEvent, ProjectEventHistory, ProjectEventRecord,
    ProjectEventSnapshot, ProjectRuntimeSummary, ProjectState, ProjectStatus,
};
