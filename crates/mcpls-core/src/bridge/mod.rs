//! Translation layer between MCP and LSP protocols.
//!
//! This module handles the bidirectional conversion between
//! MCP tool calls and LSP requests/responses.

use std::sync::{Mutex as StdMutex, MutexGuard, PoisonError};

pub(crate) mod ast_grep;
mod encoding;
mod notifications;
pub mod resources;
mod state;
pub(crate) mod translator;

pub(crate) use ast_grep::{StructuralMatch, StructuralSearchResult};
pub use encoding::{EncodingConverter, PositionEncoding, lsp_to_mcp_position, mcp_to_lsp_position};
pub use notifications::{
    DiagnosticInfo, LogEntry, LogLevel, MessageType, NotificationCache, ServerMessage,
};
pub use resources::ResourceSubscriptions;
pub(crate) use state::try_path_to_uri;
pub use state::{
    DEFAULT_MAX_DOCUMENTS, DEFAULT_MAX_FILE_SIZE, DocumentState, DocumentTracker, ResourceLimits,
    path_to_uri, uri_to_path,
};
pub use translator::{
    ActivationHealth, CallHierarchyPrepareResult, CodeActionsResult, Completion, CompletionsResult,
    DefinitionResult, Diagnostic, DiagnosticSeverity, DiagnosticsResult, DocumentChanges,
    DocumentSymbolOptions, DocumentSymbolsResult, FormatDocumentResult, HoverResult,
    IncomingCallsResult, InlayHintsResult, InspectCalls, InspectSection,
    InspectSectionCompleteness, InspectSymbolBudget, InspectSymbolRequest, InspectSymbolResolution,
    InspectSymbolResult, InspectSymbolSectionKind, InspectSymbolSections, Location,
    LocationsResult, OutgoingCallsResult, Position2D, ProjectActivation, ProviderSynchronization,
    Range, ReferencesResult, RenameResult, SemanticDiscoveryKind, SemanticDiscoveryResult,
    SemanticResultLimits, ServerCapability, ServerLogsResult, ServerMessagesResult,
    SignatureHelpResult, SourceContext, SupportedWorkspaceEdit, Symbol, SymbolHandle, TextEdit,
    Translator, TranslatorTemplate, WillRenameFilesResult, WorkspaceSymbolMatchMode,
    WorkspaceSymbolResult, WorkspaceSymbolScope, convert_code_action_or_command,
};

/// Lock a `std::sync::Mutex`, recovering the guard if a previous holder
/// panicked while holding it.
///
/// Every lock guarded this way protects a short, synchronous, panic-free
/// critical section (a `HashMap`/`HashSet` lookup or insert), so poisoning
/// can only happen if an unrelated bug already panicked; refusing to unwind
/// the whole process a second time over stale poisoning is preferable to
/// deadlocking future calls. Shared by `translator` and `state` so both
/// modules lock their interior `HashMap`/`HashSet` fields the same way.
pub(crate) fn lock_std<T>(mutex: &StdMutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
