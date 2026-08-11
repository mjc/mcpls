//! MCP tool definitions and handlers.
//!
//! This module defines the MCP tools that expose LSP capabilities
//! to AI agents.

mod handlers;
mod instrumentation;
mod server;
mod session;
mod tools;

pub(crate) use instrumentation::InstrumentedServer;
pub use server::McplsServer;
pub use tools::{
    CallHierarchyCallsParams, CallHierarchyPrepareParams, CodeActionApplyParams,
    CodeActionListParams, CodeActionPreviewParams, CompletionsParams, DefinitionParams,
    DiagnosticsParams, DocumentSymbolsParams, FormatDocumentParams, FormatPreviewParams,
    HoverParams, MoveInlineModulePreviewParams, MoveItemPreviewParams, PathRenamePreviewParams,
    ProjectAddParams, ProjectIdParams, ProjectListParams, RangeFormatPreviewParams,
    ReferencesParams, RenameParams, RenamePreviewParams, SemanticPositionParams,
    StructuralReplacePreviewParams, WorkspaceEditApplyParams, WorkspaceEditPreviewParams,
    WorkspaceSymbolParams,
};
