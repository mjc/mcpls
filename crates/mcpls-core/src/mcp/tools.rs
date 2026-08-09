//! MCP tool parameter definitions.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::bridge::{
    DocumentSymbolOptions, SymbolHandle, WorkspaceSymbolMatchMode, WorkspaceSymbolScope,
};

/// Parameters for the `get_hover` tool.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[schemars(description = "Parameters for getting hover information at a position in a file.")]
pub struct HoverParams {
    /// Absolute path to the file.
    #[schemars(description = "Absolute path to the file.")]
    #[serde(default)]
    pub file_path: String,
    /// Line number (1-based).
    #[schemars(description = "Line number (1-based).")]
    #[serde(default)]
    pub line: u32,
    /// Character/column number (1-based).
    #[schemars(description = "Character/column number (1-based).")]
    #[serde(default)]
    pub character: u32,
    /// Project owning `symbol_handle` when coordinates are omitted.
    pub project_id: Option<String>,
    /// Snapshot-bound handle returned by a prior semantic result.
    pub symbol_handle: Option<SymbolHandle>,
}

/// Parameters for the `get_definition` tool.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[schemars(description = "Parameters for getting the definition location of a symbol.")]
pub struct DefinitionParams {
    /// Absolute path to the file.
    #[schemars(description = "Absolute path to the file.")]
    #[serde(default)]
    pub file_path: String,
    /// Line number (1-based).
    #[schemars(description = "Line number (1-based).")]
    #[serde(default)]
    pub line: u32,
    /// Character/column number (1-based).
    #[schemars(description = "Character/column number (1-based).")]
    #[serde(default)]
    pub character: u32,
    /// Project owning `symbol_handle` when coordinates are omitted.
    pub project_id: Option<String>,
    /// Snapshot-bound handle returned by a prior semantic result.
    pub symbol_handle: Option<SymbolHandle>,
}

/// Project-scoped position for read-only semantic discovery.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SemanticPositionParams {
    /// Registered project that owns the file.
    pub project_id: String,
    /// Absolute path within that project.
    #[serde(default)]
    pub file_path: String,
    /// One-based line number.
    #[serde(default)]
    pub line: u32,
    /// One-based character offset in the active server's negotiated encoding.
    #[serde(default)]
    pub character: u32,
    /// Snapshot-bound handle returned by a prior semantic result.
    pub symbol_handle: Option<SymbolHandle>,
}

/// Parameters for the `get_references` tool.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[schemars(description = "Parameters for finding all references to a symbol.")]
pub struct ReferencesParams {
    /// Absolute path to the file.
    #[schemars(description = "Absolute path to the file.")]
    #[serde(default)]
    pub file_path: String,
    /// Line number (1-based).
    #[schemars(description = "Line number (1-based).")]
    #[serde(default)]
    pub line: u32,
    /// Character/column number (1-based).
    #[schemars(description = "Character/column number (1-based).")]
    #[serde(default)]
    pub character: u32,
    /// Project owning `symbol_handle` when coordinates are omitted.
    pub project_id: Option<String>,
    /// Snapshot-bound handle returned by a prior semantic result.
    pub symbol_handle: Option<SymbolHandle>,
    /// Whether to include the declaration in the results.
    #[schemars(description = "Whether to include the declaration in the results.")]
    #[serde(default)]
    pub include_declaration: bool,
}

/// Parameters for the `get_diagnostics` tool.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[schemars(description = "Parameters for getting diagnostics (errors, warnings) for a file.")]
pub struct DiagnosticsParams {
    /// Absolute path to the file.
    #[schemars(description = "Absolute path to the file.")]
    pub file_path: String,
}

/// Parameters for the `rename_symbol` tool.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[schemars(description = "Parameters for renaming a symbol across the workspace.")]
pub struct RenameParams {
    /// Absolute path to the file.
    #[schemars(description = "Absolute path to the file.")]
    pub file_path: String,
    /// Line number (1-based).
    #[schemars(description = "Line number (1-based).")]
    pub line: u32,
    /// Character/column number (1-based).
    #[schemars(description = "Character/column number (1-based).")]
    pub character: u32,
    /// New name for the symbol.
    #[schemars(description = "New name for the symbol.")]
    pub new_name: String,
}

/// Parameters for previewing an LSP rename as a generic workspace edit plan.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[schemars(description = "Parameters for previewing a symbol rename as a workspace edit plan.")]
pub struct RenamePreviewParams {
    /// Registered project that owns the file.
    pub project_id: String,
    /// Absolute path to the file.
    pub file_path: String,
    /// Line number (1-based).
    pub line: u32,
    /// Character/column number (1-based).
    pub character: u32,
    /// New name for the symbol.
    pub new_name: String,
    /// Optional negotiated LSP position encoding.
    #[serde(default)]
    pub position_encoding: Option<String>,
}

/// Parameters for the `get_completions` tool.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[schemars(description = "Parameters for getting code completion suggestions.")]
pub struct CompletionsParams {
    /// Absolute path to the file.
    #[schemars(description = "Absolute path to the file.")]
    pub file_path: String,
    /// Line number (1-based).
    #[schemars(description = "Line number (1-based).")]
    pub line: u32,
    /// Character/column number (1-based).
    #[schemars(description = "Character/column number (1-based).")]
    pub character: u32,
    /// Optional trigger character (e.g., '.', ':', '->').
    #[schemars(description = "Optional trigger character (e.g., '.', ':', '->').")]
    pub trigger: Option<String>,
}

/// Parameters for the `get_document_symbols` tool.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[schemars(description = "Parameters for getting all symbols in a document.")]
pub struct DocumentSymbolsParams {
    /// Absolute path to the file.
    #[schemars(description = "Absolute path to the file.")]
    pub file_path: String,
    /// Optional query, filters, hierarchy bounds, and body controls.
    #[serde(flatten)]
    pub options: DocumentSymbolOptions,
}

/// Parameters for the `format_document` tool.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[schemars(description = "Parameters for formatting a document.")]
pub struct FormatDocumentParams {
    /// Absolute path to the file.
    #[schemars(description = "Absolute path to the file.")]
    pub file_path: String,
    /// Tab size for formatting (default: 4).
    #[schemars(description = "Tab size for formatting (default: 4).")]
    #[serde(default = "default_tab_size")]
    pub tab_size: u32,
    /// Whether to use spaces instead of tabs (default: true).
    #[schemars(description = "Whether to use spaces instead of tabs (default: true).")]
    #[serde(default = "default_insert_spaces")]
    pub insert_spaces: bool,
}

/// Parameters for previewing document formatting as a generic workspace edit plan.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[schemars(description = "Parameters for previewing formatting as a workspace edit plan.")]
pub struct FormatPreviewParams {
    /// Registered project that owns the file.
    pub project_id: String,
    /// Absolute path to the file.
    pub file_path: String,
    /// Tab size for formatting (default: 4).
    #[serde(default = "default_tab_size")]
    pub tab_size: u32,
    /// Whether to use spaces instead of tabs (default: true).
    #[serde(default = "default_insert_spaces")]
    pub insert_spaces: bool,
    /// Optional negotiated LSP position encoding.
    #[serde(default)]
    pub position_encoding: Option<String>,
}

/// Parameters for previewing standard LSP range formatting.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RangeFormatPreviewParams {
    /// Registered project that owns the file.
    pub project_id: String,
    /// Absolute path to format.
    pub file_path: String,
    /// One-based start line.
    pub start_line: u32,
    /// One-based start character in UTF-16 code units.
    pub start_character: u32,
    /// One-based end line.
    pub end_line: u32,
    /// One-based end character in UTF-16 code units.
    pub end_character: u32,
    /// Formatting tab size.
    #[serde(default = "default_tab_size")]
    pub tab_size: u32,
    /// Whether indentation uses spaces.
    #[serde(default = "default_insert_spaces")]
    pub insert_spaces: bool,
    /// Optional negotiated LSP position encoding.
    #[serde(default)]
    pub position_encoding: Option<String>,
}

/// Parameters for previewing rust-analyzer item movement.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct MoveItemPreviewParams {
    /// Registered project that owns the file.
    pub project_id: String,
    /// Absolute Rust source path.
    pub file_path: String,
    /// One-based selection start line.
    pub start_line: u32,
    /// One-based selection start character.
    pub start_character: u32,
    /// One-based selection end line.
    pub end_line: u32,
    /// One-based selection end character.
    pub end_character: u32,
    /// `up` or `down`.
    pub direction: String,
    /// Optional negotiated LSP position encoding.
    #[serde(default)]
    pub position_encoding: Option<String>,
}

/// Parameters for previewing a Rust inline-module move.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[schemars(description = "Parameters for moving one inline Rust module to its own file.")]
pub struct MoveInlineModulePreviewParams {
    /// Registered project that owns the source file.
    pub project_id: String,
    /// Absolute path to the Rust source containing the inline module.
    pub file_path: String,
    /// Inline module name to move.
    pub module_name: String,
    /// Optional zero-based LSP line containing the selected module declaration.
    #[serde(default)]
    pub module_line: Option<u32>,
    /// Optional zero-based LSP character within the selected module declaration.
    #[serde(default)]
    pub module_character: Option<u32>,
    /// Optional negotiated LSP position encoding.
    #[serde(default)]
    pub position_encoding: Option<String>,
}

/// Parameters for previewing one filesystem path rename with semantic updates.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[schemars(description = "Parameters for a preview-first project path rename.")]
pub struct PathRenamePreviewParams {
    /// Registered project that owns both paths and the resulting plan.
    pub project_id: String,
    /// Existing absolute file or directory path.
    pub old_path: String,
    /// Non-existing absolute destination path.
    pub new_path: String,
    /// Position encoding for language-server text edits.
    #[serde(default)]
    pub position_encoding: Option<String>,
}

/// Parameters for structural search and replacement preview.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[schemars(description = "Parameters for explicit-dialect structural search and replacement.")]
pub struct StructuralReplacePreviewParams {
    /// Registered project that owns the context file and resulting plan.
    pub project_id: String,
    /// Absolute context file used for project containment and rust-analyzer selection.
    pub file_path: String,
    /// Exact syntax dialect: `rust_analyzer_ssr` or `ast_grep`.
    pub dialect: String,
    /// Exact query in the selected dialect; MCPLS never translates it.
    pub query: String,
    /// ast-grep replacement template. Omit for search-only requests.
    #[serde(default)]
    pub replacement: Option<String>,
    /// Explicit ast-grep language ID. Required only for the `ast_grep` dialect.
    #[serde(default)]
    pub language_id: Option<String>,
    /// Validate dialect syntax without searching or constructing a plan.
    #[serde(default)]
    pub parse_only: bool,
    /// Position encoding for ast-grep ranges and workspace-edit planning.
    #[serde(default)]
    pub position_encoding: Option<String>,
}

const fn default_tab_size() -> u32 {
    4
}

const fn default_insert_spaces() -> bool {
    true
}

/// Parameters for the `workspace_symbol_search` tool.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[schemars(description = "Parameters for searching symbols across the workspace.")]
pub struct WorkspaceSymbolParams {
    /// Stable project identifier whose workspace should be searched.
    #[schemars(description = "Registered project identifier whose workspace should be searched.")]
    pub project_id: String,
    /// Search query for symbol names (supports partial matching).
    #[schemars(description = "Search query for symbol names (supports partial matching).")]
    pub query: String,
    /// Optional filter by symbol kind (function, class, variable, etc.).
    #[schemars(description = "Optional filter by symbol kind (function, class, variable, etc.).")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind_filter: Option<String>,
    /// Name matching behavior; defaults to exact-first fuzzy search.
    #[serde(default)]
    pub match_mode: WorkspaceSymbolMatchMode,
    /// Source scope; dependencies and external symbols require explicit `all`.
    #[serde(default)]
    pub scope: WorkspaceSymbolScope,
    /// Maximum results to return (default: 100).
    #[schemars(description = "Maximum results to return (default: 100).")]
    #[serde(default = "default_max_results")]
    pub limit: u32,
}

const fn default_max_results() -> u32 {
    100
}

/// Parameters for the `get_code_actions` tool.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[schemars(
    description = "Parameters for getting available code actions (quick fixes, refactorings) for a range."
)]
pub struct CodeActionsParams {
    /// Absolute path to the file.
    #[schemars(description = "Absolute path to the file.")]
    pub file_path: String,
    /// Start line (1-based).
    #[schemars(description = "Start line (1-based).")]
    pub start_line: u32,
    /// Start character (1-based).
    #[schemars(description = "Start character (1-based).")]
    pub start_character: u32,
    /// End line (1-based).
    #[schemars(description = "End line (1-based).")]
    pub end_line: u32,
    /// End character (1-based).
    #[schemars(description = "End character (1-based).")]
    pub end_character: u32,
    /// Optional filter by action kind (quickfix, refactor, source, etc.).
    #[schemars(description = "Optional filter by action kind (quickfix, refactor, source, etc.).")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind_filter: Option<String>,
}

/// Parameters for listing project-scoped code actions with reusable references.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[schemars(description = "Parameters for listing project-scoped code actions.")]
pub struct CodeActionListParams {
    /// Registered project that owns the file.
    pub project_id: String,
    /// Absolute path to the file.
    pub file_path: String,
    /// Start line (1-based).
    pub start_line: u32,
    /// Start character (1-based).
    pub start_character: u32,
    /// End line (1-based).
    pub end_line: u32,
    /// End character (1-based).
    pub end_character: u32,
    /// Optional filter by action kind.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind_filter: Option<String>,
}

/// Parameters for previewing one project-scoped code action.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[schemars(description = "Parameters for previewing a stored code action.")]
pub struct CodeActionPreviewParams {
    /// Registered project that owns the action.
    pub project_id: String,
    /// Opaque action reference returned by `get_code_actions`.
    pub action_id: String,
    /// Position encoding used by the language server.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position_encoding: Option<String>,
}

/// Parameters for applying a code-action preview plan.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[schemars(description = "Parameters for applying a code-action preview plan.")]
pub struct CodeActionApplyParams {
    /// Registered project that owns the plan.
    pub project_id: String,
    /// Opaque plan ID returned by `code_action_preview`.
    pub plan_id: String,
}

/// Parameters for the `prepare_call_hierarchy` tool.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[schemars(description = "Parameters for preparing call hierarchy at a position.")]
pub struct CallHierarchyPrepareParams {
    /// Absolute path to the file.
    #[schemars(description = "Absolute path to the file.")]
    #[serde(default)]
    pub file_path: String,
    /// Line number (1-based).
    #[schemars(description = "Line number (1-based).")]
    #[serde(default)]
    pub line: u32,
    /// Character/column number (1-based).
    #[schemars(description = "Character/column number (1-based).")]
    #[serde(default)]
    pub character: u32,
    /// Project owning `symbol_handle` when coordinates are omitted.
    pub project_id: Option<String>,
    /// Snapshot-bound handle returned by a prior semantic result.
    pub symbol_handle: Option<SymbolHandle>,
}

/// Parameters for the `get_incoming_calls` and `get_outgoing_calls` tools.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[schemars(
    description = "Parameters for getting incoming or outgoing calls for a call hierarchy item."
)]
pub struct CallHierarchyCallsParams {
    /// The call hierarchy item to get calls for (from prepare response).
    #[schemars(description = "The call hierarchy item to get calls for (from prepare response).")]
    pub item: Option<serde_json::Value>,
    /// Project owning `symbol_handle` when `item` is omitted.
    pub project_id: Option<String>,
    /// Snapshot-bound handle returned by a prior semantic result.
    pub symbol_handle: Option<SymbolHandle>,
}

/// Parameters for the `get_cached_diagnostics` tool.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[schemars(
    description = "Parameters for getting cached diagnostics from LSP server notifications."
)]
pub struct CachedDiagnosticsParams {
    /// Absolute path to the file.
    #[schemars(description = "Absolute path to the file.")]
    pub file_path: String,
}

/// Parameters for the `get_server_logs` tool.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[schemars(description = "Parameters for getting recent LSP server log messages.")]
pub struct ServerLogsParams {
    /// Registered project whose language-server logs should be returned.
    #[schemars(
        description = "Registered project ID whose language-server logs should be returned."
    )]
    pub project_id: String,
    /// Maximum number of log entries to return (default: 50).
    #[schemars(description = "Maximum number of log entries to return (default: 50).")]
    #[serde(default = "default_log_limit")]
    pub limit: usize,
    /// Minimum log level to include: error, warning, info, debug.
    #[schemars(description = "Minimum log level to include: error, warning, info, debug.")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_level: Option<String>,
}

const fn default_log_limit() -> usize {
    50
}

/// Parameters for the `get_server_messages` tool.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[schemars(
    description = "Parameters for getting recent LSP server messages (showMessage notifications)."
)]
pub struct ServerMessagesParams {
    /// Registered project whose language-server messages should be returned.
    #[schemars(
        description = "Registered project ID whose language-server messages should be returned."
    )]
    pub project_id: String,
    /// Maximum number of messages to return (default: 20).
    #[schemars(description = "Maximum number of messages to return (default: 20).")]
    #[serde(default = "default_message_limit")]
    pub limit: usize,
}

/// Parameters for inspecting negotiated language-server capabilities.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[schemars(description = "Parameters for inspecting project-scoped LSP capabilities.")]
pub struct ProjectLspCapabilitiesParams {
    /// Registered project whose capabilities should be returned.
    pub project_id: String,
    /// Optional language-server identity to filter by.
    #[serde(default)]
    pub language_id: Option<String>,
}

const fn default_message_limit() -> usize {
    20
}

/// Parameters for the `get_signature_help` tool.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[schemars(description = "Parameters for getting signature help at a position in a file.")]
pub struct SignatureHelpParams {
    /// Absolute path to the file.
    #[schemars(description = "Absolute path to the file.")]
    pub file_path: String,
    /// Line number (1-based).
    #[schemars(description = "Line number (1-based).")]
    pub line: u32,
    /// Character/column number (1-based).
    #[schemars(description = "Character/column number (1-based).")]
    pub character: u32,
}

/// Parameters for the `go_to_implementation` tool.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[schemars(description = "Parameters for navigating to implementations of a symbol.")]
pub struct GoToImplementationParams {
    /// Absolute path to the file.
    #[schemars(description = "Absolute path to the file.")]
    #[serde(default)]
    pub file_path: String,
    /// Line number (1-based).
    #[schemars(description = "Line number (1-based).")]
    #[serde(default)]
    pub line: u32,
    /// Character/column number (1-based).
    #[schemars(description = "Character/column number (1-based).")]
    #[serde(default)]
    pub character: u32,
    /// Project owning `symbol_handle` when coordinates are omitted.
    pub project_id: Option<String>,
    /// Snapshot-bound handle returned by a prior semantic result.
    pub symbol_handle: Option<SymbolHandle>,
}

/// Parameters for the `go_to_type_definition` tool.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[schemars(description = "Parameters for navigating to the type definition of an expression.")]
pub struct GoToTypeDefinitionParams {
    /// Absolute path to the file.
    #[schemars(description = "Absolute path to the file.")]
    #[serde(default)]
    pub file_path: String,
    /// Line number (1-based).
    #[schemars(description = "Line number (1-based).")]
    #[serde(default)]
    pub line: u32,
    /// Character/column number (1-based).
    #[schemars(description = "Character/column number (1-based).")]
    #[serde(default)]
    pub character: u32,
    /// Project owning `symbol_handle` when coordinates are omitted.
    pub project_id: Option<String>,
    /// Snapshot-bound handle returned by a prior semantic result.
    pub symbol_handle: Option<SymbolHandle>,
}

/// Parameters for the `get_inlay_hints` tool.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[schemars(description = "Parameters for getting inlay hints in a range.")]
pub struct InlayHintsParams {
    /// Absolute path to the file.
    #[schemars(description = "Absolute path to the file.")]
    pub file_path: String,
    /// Start line (1-based).
    #[schemars(description = "Start line (1-based).")]
    pub start_line: u32,
    /// Start character (1-based).
    #[schemars(description = "Start character (1-based).")]
    pub start_character: u32,
    /// End line (1-based).
    #[schemars(description = "End line (1-based).")]
    pub end_line: u32,
    /// End character (1-based).
    #[schemars(description = "End character (1-based).")]
    pub end_character: u32,
}

/// Parameters for registering a project with the long-lived daemon.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[schemars(description = "Register a project root under a stable project ID.")]
pub struct ProjectAddParams {
    /// Stable project identifier used by subsequent lifecycle tools.
    #[schemars(description = "Stable project identifier.")]
    pub project_id: String,
    /// Existing directory to register as the project root.
    #[schemars(description = "Absolute or relative path to an existing project directory.")]
    pub root: String,
    /// Optional project-specific actor configuration. Supported fields are
    /// `lsp_servers` and `heuristics_max_depth`; omitted fields inherit daemon
    /// defaults.
    #[serde(default)]
    #[schemars(description = "Optional project-specific configuration.")]
    pub config: Option<serde_json::Value>,
}

/// Parameters selecting a registered project.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[schemars(description = "Select a registered project by stable ID.")]
pub struct ProjectIdParams {
    /// Stable project identifier.
    pub project_id: String,
}

/// Parameters previewing an LSP workspace edit for one project.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[schemars(description = "Preview a project-scoped LSP WorkspaceEdit without changing files.")]
pub struct WorkspaceEditPreviewParams {
    /// Stable project identifier that owns the workspace roots and plan.
    pub project_id: String,
    /// LSP `WorkspaceEdit` object returned by a language server.
    pub workspace_edit: serde_json::Value,
    /// Negotiated LSP position encoding, defaulting to UTF-8 for this API.
    #[serde(default)]
    pub position_encoding: Option<String>,
}

/// Parameters applying one project-owned workspace edit plan.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[schemars(description = "Apply one previously previewed project-owned workspace edit plan.")]
pub struct WorkspaceEditApplyParams {
    /// Stable project identifier that owns the plan.
    pub project_id: String,
    /// Opaque plan identifier returned by the preview flow.
    pub plan_id: String,
}

/// Empty parameters for listing all registered projects.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[schemars(description = "List all registered projects.")]
pub struct ProjectListParams {}

/// Empty parameters for listing this MCP session's resource subscriptions.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[schemars(description = "List resource subscriptions owned by this MCP session.")]
pub struct SubscriptionListParams {}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn semantic_params_accept_a_handle_without_coordinates() {
        let handle = SymbolHandle::new();
        let params: ReferencesParams = serde_json::from_value(serde_json::json!({
            "project_id": "project",
            "symbol_handle": handle,
        }))
        .unwrap();
        assert!(params.file_path.is_empty());
        assert_eq!(params.line, 0);
        assert_eq!(params.character, 0);
    }

    #[test]
    fn semantic_param_schema_does_not_require_coordinates() {
        let schema = serde_json::to_value(schemars::schema_for!(ReferencesParams)).unwrap();
        let required = schema["required"].as_array().cloned().unwrap_or_default();
        assert!(!required.iter().any(|field| field == "file_path"));
        assert!(!required.iter().any(|field| field == "line"));
        assert!(!required.iter().any(|field| field == "character"));
    }
}
