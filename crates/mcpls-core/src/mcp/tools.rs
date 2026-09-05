//! MCP tool parameter definitions.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::bridge::{
    DeferredResourceReference, DocumentSymbolOptions, InspectSymbolBudget,
    InspectSymbolSectionKind, Range, SemanticResultLimits, SymbolHandle, WorkspaceSymbolMatchMode,
    WorkspaceSymbolScope,
};
use crate::bridge::{LexicalCaseMode, LexicalMatchMode};

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
    /// Reuse the handle from a source-bearing discovery result; refresh discovery on stale handles.
    pub project_id: Option<String>,
    /// Snapshot-bound handle returned by a prior semantic result; safer than copying coordinates.
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
    /// Snapshot-bound handle returned by a source-bearing result; refresh discovery if stale.
    pub symbol_handle: Option<SymbolHandle>,
    /// Snapshot-bound continuation returned by a prior definition response.
    #[serde(default)]
    pub page_token: Option<String>,
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
    /// Snapshot-bound handle returned by a source-bearing result; refresh discovery if stale.
    pub symbol_handle: Option<SymbolHandle>,
    /// Snapshot-bound continuation returned by a prior discovery response.
    #[serde(default)]
    pub page_token: Option<String>,
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
    /// Snapshot-bound handle returned by a source-bearing result; refresh discovery if stale.
    pub symbol_handle: Option<SymbolHandle>,
    /// Whether to include the declaration in the results.
    #[schemars(description = "Whether to include the declaration in the results.")]
    #[serde(default)]
    pub include_declaration: bool,
    /// Bounds applied to references and groups.
    #[serde(default)]
    pub limits: SemanticResultLimits,
    /// Decimal offset returned by a prior response's `next_cursor`.
    #[serde(default)]
    pub page_token: Option<String>,
}

/// Parameters for the `get_diagnostics` tool.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[schemars(description = "Parameters for getting diagnostics (errors, warnings) for a file.")]
pub struct DiagnosticsParams {
    /// Absolute path to the file.
    #[schemars(description = "Absolute path to the file.")]
    pub file_path: String,
    /// Whether to prefer cached diagnostics, force fresh analysis, or only read the cache.
    #[serde(default)]
    pub mode: DiagnosticsMode,
    /// Backward-compatible alias for `mode=fresh`; omitted from the canonical schema.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(skip)]
    pub fresh: Option<bool>,
    /// Filters, grouping behavior, and response bounds.
    #[serde(flatten)]
    pub options: crate::bridge::translator::DiagnosticOptions,
}

/// Source policy for `get_diagnostics`.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticsMode {
    /// Use the cached-notification behavior that existing `get_diagnostics` callers receive.
    #[default]
    CachedPreferred,
    /// Always request fresh analysis from the routed language server.
    Fresh,
    /// Read only server-notification diagnostics without requesting analysis.
    CacheOnly,
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
    /// Snapshot-bound continuation returned by a prior response.
    #[serde(default)]
    pub page_token: Option<String>,
}

/// Parameters for the `get_document_symbols` tool.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[schemars(description = "Parameters for a bounded, queryable document semantic outline.")]
pub struct DocumentSymbolsParams {
    /// Absolute path to the file.
    #[schemars(description = "Absolute path to the file.")]
    pub file_path: String,
    /// Optional query, filters, hierarchy bounds, and body controls.
    #[serde(flatten)]
    pub options: DocumentSymbolOptions,
    /// Maximum serialized bytes returned on this page.
    #[serde(default = "default_workspace_symbol_batch_bytes")]
    pub max_bytes: usize,
    /// Snapshot-owned continuation returned by a prior response.
    #[serde(default)]
    pub page_token: Option<String>,
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
    /// Optional one-based range for capability-gated LSP range formatting.
    #[serde(default)]
    pub range: Option<FormatRange>,
}

/// One-based range used by `format_preview`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FormatRange {
    /// One-based start line.
    pub start_line: u32,
    /// One-based start character in UTF-16 code units.
    pub start_character: u32,
    /// One-based end line.
    pub end_line: u32,
    /// One-based end character in UTF-16 code units.
    pub end_character: u32,
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
    pub query: Option<String>,
    /// Caller-ordered queries. Exact duplicates reuse the first provider result.
    #[serde(default)]
    pub queries: Vec<String>,
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
    /// Maximum results to return on one page (default: 100).
    #[schemars(description = "Maximum results to return on one page (default: 100).")]
    #[serde(default = "default_max_results")]
    pub limit: u32,
    /// Maximum serialized response bytes (default: 16384).
    #[serde(default = "default_workspace_symbol_batch_bytes")]
    pub max_bytes: usize,
    /// Snapshot-owned continuation returned by the preceding page.
    #[serde(default)]
    pub page_token: Option<String>,
    /// Include symbols under generated/build-output directories (default: false).
    #[serde(default)]
    pub include_generated: bool,
}

/// Parameters for bounded project-scoped lexical search.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LexicalSearchParams {
    /// Registered project identifier whose snapshots should be searched.
    pub project_id: String,
    /// Literal text or Rust regex to find.
    #[serde(default)]
    pub query: Option<String>,
    /// Caller-ordered queries sharing one source scan and response budget.
    #[serde(default)]
    pub queries: Vec<String>,
    /// Interpret `query` literally or as a Rust regex.
    pub mode: LexicalMatchMode,
    /// Case sensitivity behavior.
    pub case: LexicalCaseMode,
    /// Enable multiline regex anchors.
    #[serde(default)]
    pub multiline: bool,
    /// Maximum project files inspected (default: 1024).
    #[serde(default = "default_lexical_max_files")]
    pub max_files: usize,
    /// Maximum matches returned (default: 100).
    #[serde(default = "default_lexical_max_matches")]
    pub max_matches: usize,
    /// Caller byte ceiling; the server returns at most 16384 bytes per page.
    #[serde(default = "default_lexical_max_bytes")]
    pub max_bytes: usize,
    /// Opaque snapshot-owned cursor returned by a prior lexical-search response.
    #[serde(default)]
    pub page_token: Option<String>,
    /// Context lines around each match; zero returns references only.
    #[serde(default)]
    pub context_lines: usize,
    /// Include generated/build-output files.
    #[serde(default)]
    pub include_generated: bool,
    /// Optional project-relative globs to include.
    #[serde(default)]
    pub include_paths: Vec<String>,
    /// Project-relative globs to exclude after inclusion.
    #[serde(default)]
    pub exclude_paths: Vec<String>,
}

const fn default_lexical_max_files() -> usize {
    1024
}
const fn default_lexical_max_matches() -> usize {
    100
}
const fn default_lexical_max_bytes() -> usize {
    LEXICAL_PAGE_BYTES
}

pub(crate) const LEXICAL_PAGE_BYTES: usize = 16 * 1024;

/// Parameters for one bounded batch of workspace-symbol searches.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WorkspaceSymbolBatchParams {
    /// Stable project identifier whose workspace should be searched.
    pub project_id: String,
    /// Caller-ordered queries. Exact duplicates reuse the first result.
    pub queries: Vec<String>,
    /// Optional symbol-kind filter shared by every query.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind_filter: Option<String>,
    /// Name matching behavior shared by every query.
    #[serde(default)]
    pub match_mode: WorkspaceSymbolMatchMode,
    /// Source scope shared by every query.
    #[serde(default)]
    pub scope: WorkspaceSymbolScope,
    /// Maximum symbols returned across the batch.
    #[serde(default = "default_max_results")]
    pub max_items: u32,
    /// Maximum serialized response bytes.
    #[serde(default = "default_workspace_symbol_batch_bytes")]
    pub max_bytes: usize,
    /// Include symbols under generated/build-output directories.
    #[serde(default)]
    pub include_generated: bool,
}

/// Parameters for a bounded, project-scoped symbol inspection bundle.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InspectSymbolParams {
    /// Stable project identifier whose symbol should be inspected.
    pub project_id: String,
    /// Snapshot-bound handle returned by symbol discovery; preferred over repeating coordinates.
    pub symbol_handle: Option<SymbolHandle>,
    /// Caller-ordered symbol identities. Use this instead of a single query or handle for 1-16 inspections.
    #[serde(default)]
    pub targets: Vec<crate::bridge::InspectSymbolTarget>,
    /// Exact symbol name to resolve when no handle is supplied.
    pub query: Option<String>,
    /// Optional symbol-kind disambiguator.
    pub kind: Option<String>,
    /// Optional project-relative path disambiguator for duplicate names.
    pub path: Option<String>,
    /// Optional container-name disambiguator for duplicate names.
    pub container: Option<String>,
    /// Maximum ranked source-bearing candidates returned for ambiguous queries.
    #[serde(default = "default_inspect_candidates")]
    pub candidate_limit: u32,
    /// Sections to include; empty returns only the declaration source frame.
    #[serde(default)]
    pub sections: Vec<InspectSymbolSectionKind>,
    /// Caller upper bounds for the complete bundle; standalone responses are capped at 16 KiB.
    #[serde(default)]
    pub budget: InspectSymbolBudget,
    /// Opaque cursor returned by a previous multi-symbol inspection page.
    #[serde(default)]
    pub page_token: Option<String>,
}

/// Parameters for one bounded batch of symbol inspections.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InspectSymbolBatchParams {
    /// Stable project identifier whose symbols should be inspected.
    pub project_id: String,
    /// Caller-ordered symbol identities; every target is retained in the response.
    #[serde(default)]
    pub targets: Vec<crate::bridge::InspectSymbolTarget>,
    /// Maximum ranked candidates returned for each ambiguous query.
    #[serde(default = "default_inspect_candidates")]
    pub candidate_limit: u32,
    /// Sections requested for every target.
    #[serde(default)]
    pub sections: Vec<InspectSymbolSectionKind>,
    /// Shared collection bounds; serialized result pages are capped at 16 KiB.
    #[serde(default = "default_inspect_batch_budget")]
    pub budget: InspectSymbolBudget,
    /// Opaque cursor returned by the previous batch page; omit targets when continuing.
    pub page_token: Option<String>,
}

const fn default_inspect_candidates() -> u32 {
    10
}

const fn default_inspect_batch_budget() -> InspectSymbolBudget {
    InspectSymbolBudget {
        max_bytes: 128 * 1024,
        max_items: 20,
    }
}

const fn default_max_results() -> u32 {
    100
}

const fn default_workspace_symbol_batch_bytes() -> usize {
    16 * 1024
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
    /// Snapshot-bound continuation returned by a prior response.
    #[serde(default)]
    pub page_token: Option<String>,
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
    /// Snapshot-bound continuation returned by a prior response.
    #[serde(default)]
    pub page_token: Option<String>,
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
    /// Optional time to wait for a contending edit before returning `not_ready`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait_timeout_ms: Option<u64>,
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
    /// Snapshot-bound cursor returned by a previous page.
    pub page_token: Option<String>,
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
    /// Bounds applied to call groups and call sites.
    #[serde(default)]
    pub limits: SemanticResultLimits,
    /// Snapshot-bound continuation cursor returned by a prior page.
    #[serde(default)]
    pub page_token: Option<String>,
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
    /// Filters, grouping behavior, and response bounds.
    #[serde(flatten)]
    pub options: crate::bridge::translator::DiagnosticOptions,
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
    /// Snapshot-bound continuation cursor returned by a prior page.
    #[serde(default)]
    pub cursor: Option<String>,
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
    /// Snapshot-bound continuation cursor returned by a prior page.
    #[serde(default)]
    pub cursor: Option<String>,
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
    /// Snapshot-bound continuation returned by a prior response.
    #[serde(default)]
    pub page_token: Option<String>,
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
    /// Snapshot-bound continuation returned by a prior implementation response.
    #[serde(default)]
    pub page_token: Option<String>,
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
    /// Snapshot-bound continuation returned by a prior type-definition response.
    #[serde(default)]
    pub page_token: Option<String>,
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
    /// Snapshot-bound continuation returned by a prior inlay-hints response.
    #[serde(default)]
    pub page_token: Option<String>,
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

/// Parameters for replacing one project's Rust Cargo feature profile.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[schemars(description = "Replace a registered project's rust-analyzer Cargo feature profile.")]
pub struct ProjectCargoFeaturesParams {
    /// Stable project identifier.
    pub project_id: String,
    /// Explicit Cargo feature names.
    #[serde(default)]
    pub features: Vec<String>,
    /// Ask Cargo to enable every feature.
    #[serde(default)]
    pub all_features: bool,
    /// Ask Cargo to disable default features.
    #[serde(default)]
    pub no_default_features: bool,
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
    /// Optional time to wait for a contending edit before returning `not_ready`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait_timeout_ms: Option<u64>,
}

/// Outcome of applying a project-owned workspace edit plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum WorkspaceEditApplyResult {
    /// The plan was committed atomically.
    Applied {
        /// Stable project identifier that owned the plan.
        project_id: String,
        /// Opaque identifier of the committed plan.
        plan_id: String,
        /// Whether the edit reached the filesystem commit point.
        committed: bool,
        /// Project-relative paths committed by the edit.
        committed_files: Vec<String>,
        /// Total committed file count, including paths omitted from this response.
        committed_file_count: usize,
        /// Human-readable operations captured by the preview.
        operations: Vec<String>,
        /// Total operation count, including operations omitted from this response.
        operation_count: usize,
        /// Unified diff captured by the preview.
        unified_diff: String,
        /// Session-private resource for complete applied detail when the inline diff is bounded.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail_resource: Option<String>,
        /// Optional semantic verification outcome.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        verification: Option<String>,
        /// Optional post-commit provider convergence details.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        provider_synchronization: Vec<WorkspaceEditProviderSynchronization>,
        /// Total provider synchronization result count, including omitted results.
        provider_synchronization_count: usize,
        /// Whether complete committed detail is available from `detail_resource`.
        details_truncated: bool,
        /// Aggregate provider state when synchronization details are present.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        semantic_state: Option<String>,
    },
    /// Another edit currently owns at least one required path.
    NotReady {
        /// Stable machine-readable reason.
        reason: String,
        /// Stable project identifier that owns the plan.
        project_id: String,
        /// Opaque identifier of the unconsumed plan.
        plan_id: String,
        /// Whether the edit reached the filesystem commit point.
        committed: bool,
        /// Instructions for an idempotent retry.
        retry: WorkspaceEditRetry,
        /// Safe details about the overlapping edit scope.
        contention: WorkspaceEditContention,
    },
    /// The plan snapshot no longer matches one or more files.
    Conflict {
        /// Stable machine-readable reason.
        reason: String,
        /// Stable project identifier that owns the plan.
        project_id: String,
        /// Opaque identifier of the rejected plan.
        plan_id: String,
        /// Whether the edit reached the filesystem commit point.
        committed: bool,
        /// Instructions for producing a fresh plan.
        retry: WorkspaceEditRetry,
        /// Project-relative paths whose snapshots changed.
        changed_paths: Vec<String>,
    },
}

/// Post-commit convergence status for one language server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WorkspaceEditProviderSynchronization {
    /// Stable routing identity of the provider.
    pub provider: String,
    /// Whether watched files, document lifecycle, and VFS probes converged.
    pub synchronized: bool,
    /// Number of watched-file notifications flushed to the provider.
    pub watched_file_notifications: usize,
    /// Failure or degradation detail when synchronization was not proven.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// Retry instructions attached to a non-applied workspace edit result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WorkspaceEditRetry {
    /// Next operation the client should perform.
    pub action: WorkspaceEditRetryAction,
    /// Whether retrying with the current plan ID is safe.
    pub same_plan: bool,
    /// Suggested delay before retrying, when useful.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_ms: Option<u64>,
}

/// Client action recommended after a non-applied workspace edit result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceEditRetryAction {
    /// Retry applying the same plan.
    RetryApply,
    /// Preview the edit again and apply the new plan.
    PreviewAgain,
}

/// Non-sensitive description of paths held by a contending edit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WorkspaceEditContention {
    /// Scope in which the edit overlaps another operation.
    pub scope: WorkspaceEditContentionScope,
    /// Bounded project-relative paths blocking this edit.
    pub blocked_paths: Vec<String>,
    /// Total number of paths blocking this edit, including omitted paths.
    pub blocked_path_count: usize,
}

/// Scope in which workspace edit contention was detected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceEditContentionScope {
    /// Contention between sessions editing one canonical worktree.
    SameWorktree,
}

/// Parameters for listing all registered projects.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[schemars(description = "List all registered projects.")]
pub struct ProjectListParams {
    /// Snapshot-bound cursor returned by a prior `project_list` response.
    #[serde(default)]
    pub cursor: Option<String>,
}

/// Empty parameters for daemon health and status snapshots.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[schemars(description = "Return a daemon health or status snapshot.")]
pub struct DaemonStatusParams {}

/// Parameters for listing this MCP session's resource subscriptions.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[schemars(description = "List resource subscriptions owned by this MCP session.")]
pub struct SubscriptionListParams {
    /// Snapshot-bound continuation cursor returned by a prior page.
    #[serde(default)]
    pub cursor: Option<String>,
}

/// Parameters for reading source or deferred semantic context through a tool call.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SemanticResourceReadParams {
    /// `mcpls-source://` or `mcpls-deferred://` URI returned by another MCPLS tool.
    pub uri: String,
}

/// Tool-call representation of one semantic resource page.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SemanticResourceReadResult {
    /// Resource URI that was read.
    pub uri: String,
    /// MIME type of `text`. Source resources use a text MIME type; deferred resources use JSON.
    pub mime_type: String,
    /// Raw source text for a source resource, or an ordered UTF-8 JSON fragment for a deferred resource.
    pub text: String,
    /// Structured metadata for a source resource. Absent for deferred JSON resources.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<SemanticSourceMetadata>,
    /// URI for the next fragment, when this response is not the complete payload.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_uri: Option<String>,
    /// Total byte length of the complete payload when this is a fragment.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_bytes: Option<usize>,
    /// Byte offset of this fragment in the complete payload.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offset_bytes: Option<usize>,
    /// Number of payload bytes returned in this fragment.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub returned_bytes: Option<usize>,
    /// Number of payload bytes still available after this fragment.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remaining_bytes: Option<usize>,
    /// Snapshot identity of the immutable deferred payload.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot_hash: Option<String>,
}

/// Metadata accompanying raw source text returned by `read_semantic_resource`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SemanticSourceMetadata {
    pub path: String,
    pub uri: String,
    pub range: Range,
    pub highlighted_range: Range,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub document_version: Option<i32>,
    pub content_hash: String,
    pub returned_lines: usize,
    pub total_lines: usize,
    pub returned_bytes: usize,
    pub total_bytes: usize,
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource: Option<DeferredResourceReference>,
}

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

    #[test]
    fn document_outline_options_are_flattened_into_the_tool_schema() {
        let params: DocumentSymbolsParams = serde_json::from_value(serde_json::json!({
            "file_path": "/tmp/lib.rs",
            "query": "run",
            "match_mode": "exact",
            "include_private": true
        }))
        .unwrap();
        assert_eq!(params.options.query.as_deref(), Some("run"));

        let schema = serde_json::to_value(schemars::schema_for!(DocumentSymbolsParams)).unwrap();
        assert!(schema["properties"]["query"].is_object());
        assert!(schema["properties"]["include_bodies"].is_object());
        assert!(schema["properties"]["options"].is_null());
    }

    #[test]
    fn inspect_symbol_accepts_section_selection_and_total_budget() {
        let params: InspectSymbolParams = serde_json::from_value(serde_json::json!({
            "project_id": "project",
            "query": "run",
            "sections": ["declaration", "references", "diagnostics"],
            "budget": {"max_bytes": 8192, "max_items": 7}
        }))
        .unwrap();

        assert_eq!(params.sections.len(), 3);
        assert_eq!(params.budget.max_bytes, 8192);
        assert_eq!(params.budget.max_items, 7);
    }

    #[test]
    fn inspect_symbol_rejects_ignored_budget_and_section_fields() {
        for params in [
            serde_json::json!({
                "project_id": "project",
                "query": "run",
                "max_chars": 18_000
            }),
            serde_json::json!({
                "project_id": "project",
                "query": "run",
                "max_bytes": 18_000
            }),
            serde_json::json!({
                "project_id": "project",
                "query": "run",
                "section": "declaration"
            }),
            serde_json::json!({
                "project_id": "project",
                "query": "run",
                "budget": {"byte_budget": 18_000}
            }),
        ] {
            let error = serde_json::from_value::<InspectSymbolParams>(params).unwrap_err();
            assert!(error.to_string().contains("unknown field"));
        }
    }

    #[test]
    fn inspect_symbol_batch_rejects_unknown_target_and_budget_fields() {
        for params in [
            serde_json::json!({
                "project_id": "project",
                "targets": [{"query": "run", "section": "declaration"}]
            }),
            serde_json::json!({
                "project_id": "project",
                "targets": [{"query": "run"}],
                "budget": {"item_budget": 7}
            }),
        ] {
            let error = serde_json::from_value::<InspectSymbolBatchParams>(params).unwrap_err();
            assert!(error.to_string().contains("unknown field"));
        }
    }

    #[test]
    fn inspect_symbol_schemas_are_closed_objects() {
        for schema in [
            serde_json::to_value(schemars::schema_for!(InspectSymbolParams)).unwrap(),
            serde_json::to_value(schemars::schema_for!(InspectSymbolBatchParams)).unwrap(),
            serde_json::to_value(schemars::schema_for!(crate::bridge::InspectSymbolTarget))
                .unwrap(),
            serde_json::to_value(schemars::schema_for!(InspectSymbolBudget)).unwrap(),
        ] {
            assert_eq!(schema["additionalProperties"], false);
        }
    }

    #[test]
    fn inspect_symbol_batch_accepts_handles_under_one_shared_budget() {
        let params: InspectSymbolBatchParams = serde_json::from_value(serde_json::json!({
            "project_id": "project",
            "targets": [{"symbol_handle": "target-1"}, {"query": "run"}],
            "sections": ["declaration", "references"],
            "budget": {"max_bytes": 16384, "max_items": 4}
        }))
        .unwrap();

        assert_eq!(params.targets.len(), 2);
        assert_eq!(params.budget.max_bytes, 16_384);
        assert_eq!(params.budget.max_items, 4);
    }

    #[test]
    fn workspace_edit_apply_result_uses_explicit_statuses() {
        let applied = WorkspaceEditApplyResult::Applied {
            project_id: "project".to_owned(),
            plan_id: "plan".to_owned(),
            committed: true,
            committed_files: vec!["src/lib.rs".to_owned()],
            committed_file_count: 1,
            operations: vec!["edit src/lib.rs".to_owned()],
            operation_count: 1,
            unified_diff: "diff".to_owned(),
            detail_resource: None,
            verification: None,
            provider_synchronization: Vec::new(),
            provider_synchronization_count: 0,
            details_truncated: false,
            semantic_state: None,
        };
        let not_ready = WorkspaceEditApplyResult::NotReady {
            reason: "edit_in_progress".to_owned(),
            project_id: "project".to_owned(),
            plan_id: "plan".to_owned(),
            committed: false,
            retry: WorkspaceEditRetry {
                action: WorkspaceEditRetryAction::RetryApply,
                same_plan: true,
                after_ms: Some(100),
            },
            contention: WorkspaceEditContention {
                scope: WorkspaceEditContentionScope::SameWorktree,
                blocked_paths: vec!["src/lib.rs".to_owned()],
                blocked_path_count: 1,
            },
        };
        let conflict = WorkspaceEditApplyResult::Conflict {
            reason: "snapshot_changed".to_owned(),
            project_id: "project".to_owned(),
            plan_id: "plan".to_owned(),
            committed: false,
            retry: WorkspaceEditRetry {
                action: WorkspaceEditRetryAction::PreviewAgain,
                same_plan: false,
                after_ms: None,
            },
            changed_paths: vec!["src/lib.rs".to_owned()],
        };

        assert_eq!(
            serde_json::to_value(applied).unwrap(),
            serde_json::json!({
                "status": "applied",
                "project_id": "project",
                "plan_id": "plan",
                "committed": true,
                "committed_files": ["src/lib.rs"],
                "committed_file_count": 1,
                "operations": ["edit src/lib.rs"],
                "operation_count": 1,
                "provider_synchronization_count": 0,
                "details_truncated": false,
                "unified_diff": "diff",
            })
        );

        assert_eq!(
            serde_json::to_value(not_ready).unwrap(),
            serde_json::json!({
                "status": "not_ready",
                "reason": "edit_in_progress",
                "project_id": "project",
                "plan_id": "plan",
                "committed": false,
                "retry": {
                    "action": "retry_apply",
                    "same_plan": true,
                    "after_ms": 100,
                },
                "contention": {
                    "scope": "same_worktree",
                    "blocked_paths": ["src/lib.rs"],
                    "blocked_path_count": 1,
                },
            })
        );
        assert_eq!(
            serde_json::to_value(conflict).unwrap(),
            serde_json::json!({
                "status": "conflict",
                "reason": "snapshot_changed",
                "project_id": "project",
                "plan_id": "plan",
                "committed": false,
                "retry": {
                    "action": "preview_again",
                    "same_plan": false,
                },
                "changed_paths": ["src/lib.rs"],
            })
        );
    }

    #[test]
    fn apply_wait_timeout_is_optional() {
        let workspace: WorkspaceEditApplyParams = serde_json::from_value(serde_json::json!({
            "project_id": "project",
            "plan_id": "plan",
        }))
        .unwrap();
        let code_action: CodeActionApplyParams = serde_json::from_value(serde_json::json!({
            "project_id": "project",
            "plan_id": "plan",
            "wait_timeout_ms": 250,
        }))
        .unwrap();

        assert_eq!(workspace.wait_timeout_ms, None);
        assert_eq!(code_action.wait_timeout_ms, Some(250));

        let schema = serde_json::to_value(schemars::schema_for!(WorkspaceEditApplyParams)).unwrap();
        let required = schema["required"].as_array().cloned().unwrap_or_default();
        assert!(!required.iter().any(|field| field == "wait_timeout_ms"));
    }
}
