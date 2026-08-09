//! Public MCP-facing result/data-transfer types returned by the tool-call
//! handlers in the sibling domain modules.

use serde::{Deserialize, Serialize};

/// Opaque project-actor-owned reference to a symbol at one source snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(transparent)]
pub struct SymbolHandle(String);

impl SymbolHandle {
    /// Create a new unguessable handle.
    #[must_use]
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }
}

impl Default for SymbolHandle {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for SymbolHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

/// Position in a document (1-based for MCP).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Position2D {
    /// Line number (1-based).
    pub line: u32,
    /// Character offset (1-based).
    pub character: u32,
}

/// Range in a document (1-based for MCP).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Range {
    /// Start position.
    pub start: Position2D,
    /// End position.
    pub end: Position2D,
}

/// Location in a document.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Location {
    /// Human-readable filesystem path for file-backed targets.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// URI of the document.
    pub uri: String,
    /// Range within the document.
    pub range: Range,
    /// Bounded source text for this location, or a stable reason it is unavailable.
    pub source: SourceContext,
    /// Snapshot-bound target for coordinate-free semantic follow-ups.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol_handle: Option<SymbolHandle>,
}

/// Source context attached to a semantic result location.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum SourceContext {
    /// The source was safely resolved.
    Available(SourceFrame),
    /// The source could not be safely resolved.
    Unavailable {
        /// Stable machine-readable reason.
        reason: SourceUnavailableReason,
    },
}

/// A bounded, line-numbered source excerpt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceFrame {
    /// Canonical filesystem path.
    pub path: String,
    /// Canonical file URI.
    pub uri: String,
    /// Normalized 1-based result range.
    pub range: Range,
    /// Range highlighted by consumers; currently identical to `range`.
    pub highlighted_range: Range,
    /// Line-numbered source text.
    pub text: String,
    /// Language identifier when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language_id: Option<String>,
    /// Open-document version, when the frame came from actor-owned state.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub document_version: Option<i32>,
    /// SHA-256 of the exact content used for this frame.
    pub content_hash: String,
    /// Number of source lines returned.
    pub returned_lines: usize,
    /// Number of source lines in the resolved snapshot.
    pub total_lines: usize,
    /// Number of source bytes returned.
    pub returned_bytes: usize,
    /// Number of source bytes available in the selected line window.
    pub total_bytes: usize,
    /// Whether a line or byte budget shortened the frame.
    pub truncated: bool,
}

/// Stable reasons source text is unavailable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceUnavailableReason {
    /// The LSP returned a URI other than `file:`.
    NonFileUri,
    /// The path is outside registered workspace and approved source roots.
    OutsideApprovedRoots,
    /// The tracked or on-disk source no longer exists.
    NotFound,
    /// The source could not be read as UTF-8 text.
    Unreadable,
    /// The response-wide source budget was exhausted.
    ResponseBudgetExhausted,
}

/// Semantic relationship represented by a navigation result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NavigationKind {
    /// Hover data for the selected symbol.
    Hover,
    /// The symbol's definition.
    Definition,
    /// A declaration distinct from its definition.
    Declaration,
    /// The definition of the selected value's type.
    TypeDefinition,
    /// An implementation of the selected trait or interface member.
    Implementation,
    /// A parent Rust module.
    ParentModule,
    /// A child Rust module.
    ChildModule,
    /// A callable accepted by call-hierarchy preparation.
    CallHierarchy,
}

/// Result of a hover request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HoverResult {
    /// Stable protocol provider identity.
    pub provider: String,
    /// Relationship represented by this result.
    pub kind: NavigationKind,
    /// Hover contents as markdown string.
    pub contents: String,
    /// Optional range the hover applies to.
    pub range: Option<Range>,
    /// Bounded source surrounding the hovered target.
    pub source: SourceContext,
    /// Whether the source preview was shortened by a response budget.
    pub truncated: bool,
    /// Snapshot-bound target for coordinate-free semantic follow-ups.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol_handle: Option<SymbolHandle>,
}

/// Result of a definition request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DefinitionResult {
    /// Stable protocol provider identity.
    pub provider: String,
    /// Relationship represented by the returned targets.
    pub kind: NavigationKind,
    /// Locations of the definition.
    pub locations: Vec<Location>,
    /// Whether item or response budgets omitted target data.
    pub truncated: bool,
}

/// Result of a references request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReferencesResult {
    /// Stable protocol provider identity.
    pub provider: String,
    /// References grouped by project-relative file.
    pub groups: Vec<ReferenceGroup>,
    /// Number of references reported by the language server.
    pub total_references: usize,
    /// Number of references returned after response budgets.
    pub returned_references: usize,
    /// Whether response budgets omitted source or references.
    pub truncated: bool,
}

/// References returned from one source file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReferenceGroup {
    /// Project-relative path, or the absolute path for external sources.
    pub project_relative_path: String,
    /// Enclosing symbol when the language server can determine one safely.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enclosing_symbol: Option<String>,
    /// References within the group, in source order.
    pub references: Vec<ReferenceUse>,
}

/// One source use of the referenced symbol.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReferenceUse {
    /// Bounded location and highlighted source frame for the use.
    pub location: Location,
    /// Semantic role, reported conservatively when unavailable from the server.
    pub role: ReferenceRole,
}

/// Safely known role of a reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReferenceRole {
    /// The language server did not provide enough information to classify it.
    Unknown,
}

/// Diagnostic severity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DiagnosticSeverity {
    /// Error diagnostic.
    Error,
    /// Warning diagnostic.
    Warning,
    /// Informational diagnostic.
    Information,
    /// Hint diagnostic.
    Hint,
}

/// A single diagnostic.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    /// Range where the diagnostic applies.
    pub range: Range,
    /// Severity of the diagnostic.
    pub severity: DiagnosticSeverity,
    /// Diagnostic message.
    pub message: String,
    /// Optional diagnostic code.
    pub code: Option<String>,
}

/// Result of a diagnostics request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnosticsResult {
    /// List of diagnostics for the document.
    pub diagnostics: Vec<Diagnostic>,
}

/// A text edit operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextEdit {
    /// Range to replace.
    pub range: Range,
    /// New text.
    pub new_text: String,
}

/// Changes to a document.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocumentChanges {
    /// URI of the document.
    pub uri: String,
    /// List of edits to apply.
    pub edits: Vec<TextEdit>,
}

/// Result of a rename request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenameResult {
    /// Changes to apply across documents.
    pub changes: Vec<DocumentChanges>,
}

/// A completion item.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Completion {
    /// Label of the completion.
    pub label: String,
    /// Kind of completion.
    pub kind: Option<String>,
    /// Detail information.
    pub detail: Option<String>,
    /// Documentation.
    pub documentation: Option<String>,
}

/// Result of a completions request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletionsResult {
    /// List of completion items.
    pub items: Vec<Completion>,
}

/// A document symbol.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Symbol {
    /// Name of the symbol.
    pub name: String,
    /// Kind of symbol.
    pub kind: String,
    /// Range of the symbol.
    pub range: Range,
    /// Selection range (identifier location).
    pub selection_range: Range,
    /// Snapshot-bound target for coordinate-free semantic follow-ups.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol_handle: Option<SymbolHandle>,
    /// Parent symbol name when this is a nested declaration.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container_name: Option<String>,
    /// How this symbol matched an outline query.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub match_class: Option<WorkspaceSymbolMatch>,
    /// Stable query score derived from `match_class`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score: Option<u8>,
    /// Bounded declaration source attached after filtering.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<SourceContext>,
    /// Internal visibility classifier used by outline filters.
    #[serde(skip)]
    pub(crate) is_private: bool,
    /// Internal test classifier used by outline filters.
    #[serde(skip)]
    pub(crate) is_test: bool,
    /// Child symbols.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub children: Option<Vec<Self>>,
}

/// Query and output bounds for a document semantic outline.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DocumentSymbolOptions {
    /// Optional symbol-name query.
    pub query: Option<String>,
    /// Exact, prefix, or exact-first fuzzy matching.
    #[serde(default)]
    pub match_mode: WorkspaceSymbolMatchMode,
    /// Optional symbol-kind filter.
    pub kind_filter: Option<String>,
    /// Maximum hierarchy depth; defaults to one for compact no-query outlines.
    pub max_depth: Option<u32>,
    /// Maximum matched declarations returned.
    #[serde(default = "default_document_symbol_limit")]
    pub limit: u32,
    /// Include test modules and test declarations.
    #[serde(default)]
    pub include_tests: bool,
    /// Include declarations detected as private.
    #[serde(default)]
    pub include_private: bool,
    /// Include bounded declaration bodies instead of headers only.
    #[serde(default)]
    pub include_bodies: bool,
}

const fn default_document_symbol_limit() -> u32 {
    100
}

impl Default for DocumentSymbolOptions {
    fn default() -> Self {
        Self {
            query: None,
            match_mode: WorkspaceSymbolMatchMode::default(),
            kind_filter: None,
            max_depth: None,
            limit: default_document_symbol_limit(),
            include_tests: false,
            include_private: false,
            include_bodies: false,
        }
    }
}

impl DocumentSymbolOptions {
    /// Full bounded tree used by internal refactor validation.
    #[must_use]
    pub(crate) fn internal_tree() -> Self {
        Self {
            max_depth: Some(16),
            limit: 1_000,
            include_tests: true,
            include_private: true,
            ..Self::default()
        }
    }
}

/// Result of a document symbols request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocumentSymbolsResult {
    /// List of symbols in the document.
    pub symbols: Vec<Symbol>,
    /// Path relative to the owning project root.
    pub project_relative_path: Option<String>,
    /// Matching declarations before the result limit.
    pub total: usize,
    /// Matching declarations returned.
    pub returned: usize,
    /// Whether result or source budgets shortened the response.
    pub truncated: bool,
    /// Filters and bounds applied to this outline.
    pub filters: DocumentSymbolOptions,
}

/// Result of a format document request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FormatDocumentResult {
    /// List of edits to format the document.
    pub edits: Vec<TextEdit>,
}

/// A workspace symbol.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceSymbol {
    /// Name of the symbol.
    pub name: String,
    /// Kind of symbol.
    pub kind: String,
    /// Location of the symbol.
    pub location: Location,
    /// Optional container name (parent scope).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container_name: Option<String>,
    /// How the name matched the query.
    pub match_class: WorkspaceSymbolMatch,
    /// Stable score derived from `match_class`; larger is better.
    pub score: u8,
    /// Path relative to the registered project root, when project-local.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_relative_path: Option<String>,
    /// Whether the symbol belongs to the registered project or external source.
    pub origin: WorkspaceSymbolOrigin,
}

/// Requested workspace-symbol name matching behavior.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceSymbolMatchMode {
    /// Return only exact case-sensitive or case-insensitive names.
    Exact,
    /// Return exact and prefix matches.
    Prefix,
    /// Return exact, prefix, and fuzzy matches, ranked in that order.
    #[default]
    Fuzzy,
}

/// How a workspace-symbol name matched the query.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceSymbolMatch {
    /// Exact case-sensitive name.
    Exact,
    /// Exact name ignoring case.
    ExactCaseInsensitive,
    /// Name starts with the query, ignoring case.
    Prefix,
    /// Query characters occur in order in the name.
    Fuzzy,
}

impl WorkspaceSymbolMatch {
    /// Stable score used for result ordering and exposed to clients.
    #[must_use]
    pub const fn score(self) -> u8 {
        match self {
            Self::Exact => 100,
            Self::ExactCaseInsensitive => 90,
            Self::Prefix => 70,
            Self::Fuzzy => 50,
        }
    }
}

/// Requested workspace-symbol source scope.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceSymbolScope {
    /// Only symbols under a registered project root.
    #[default]
    Project,
    /// Include dependency and other external symbols returned by the language server.
    All,
}

/// Workspace-symbol ownership relative to the registered project.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceSymbolOrigin {
    /// Symbol source is under a registered project root.
    ProjectLocal,
    /// Symbol source is outside every registered project root.
    External,
}

/// Result of workspace symbol search.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceSymbolResult {
    /// List of symbols found.
    pub symbols: Vec<WorkspaceSymbol>,
    /// Number of matching symbols before the item budget.
    pub total: usize,
    /// Number of symbols in this response.
    pub returned: usize,
    /// Whether the item budget omitted matching symbols.
    pub truncated: bool,
}

/// A single code action.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeAction {
    /// Opaque project-scoped reference for previewing this action.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action_id: Option<String>,
    /// Title of the code action.
    pub title: String,
    /// Kind of code action (quickfix, refactor, etc.).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Diagnostics that this action resolves.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub diagnostics: Vec<Diagnostic>,
    /// Workspace edit to apply.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub edit: Option<WorkspaceEditDescription>,
    /// Lossless raw workspace edit, including resource operations.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_edit: Option<serde_json::Value>,
    /// Command to execute.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<CommandDescription>,
    /// Whether this is the preferred action.
    #[serde(default)]
    pub is_preferred: bool,
    /// LSP-disabled reason, when the action cannot currently run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disabled: Option<String>,
    /// Opaque LSP data used by `codeAction/resolve`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

/// Description of a workspace edit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceEditDescription {
    /// Changes to apply to documents.
    pub changes: Vec<DocumentChanges>,
}

/// Description of a command.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandDescription {
    /// Title of the command.
    pub title: String,
    /// Command identifier.
    pub command: String,
    /// Command arguments.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub arguments: Vec<serde_json::Value>,
}

/// Result of code actions request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeActionsResult {
    /// Available code actions.
    pub actions: Vec<CodeAction>,
}

/// A call hierarchy item.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallHierarchyItemResult {
    /// Name of the symbol.
    pub name: String,
    /// LSP numeric symbol kind (e.g. 12 for Function).
    pub kind: u32,
    /// More detail for this item.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// URI of the document.
    pub uri: String,
    /// Human-readable filesystem path for file-backed targets.
    #[serde(default, skip_deserializing, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Range of the symbol.
    pub range: Range,
    /// Selection range (identifier location).
    ///
    /// Serialized as `selectionRange` (camelCase) so that the value returned by
    /// `prepare_call_hierarchy` round-trips correctly when the MCP client passes
    /// it back to `get_incoming_calls` / `get_outgoing_calls`, which deserialize
    /// it as `lsp_types::CallHierarchyItem` (camelCase).
    #[serde(rename = "selectionRange")]
    pub selection_range: Range,
    /// Bounded source text for the callable item.
    #[serde(default, skip_deserializing, skip_serializing_if = "Option::is_none")]
    pub source: Option<SourceContext>,
    /// Snapshot-bound target for coordinate-free semantic follow-ups.
    #[serde(default, skip_deserializing, skip_serializing_if = "Option::is_none")]
    pub symbol_handle: Option<SymbolHandle>,
    /// Opaque data to pass to incoming/outgoing calls.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

/// Result of call hierarchy prepare request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallHierarchyPrepareResult {
    /// Stable protocol provider identity.
    pub provider: String,
    /// Relationship represented by the returned items.
    pub kind: NavigationKind,
    /// List of callable items at the position.
    pub items: Vec<CallHierarchyItemResult>,
    /// Whether item or response budgets omitted target data.
    pub truncated: bool,
}

/// An incoming call (caller of the current item).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IncomingCall {
    /// The item that calls the current item.
    pub from: CallHierarchyItemResult,
    /// Ranges where the call occurs.
    pub from_ranges: Vec<Range>,
}

/// Result of incoming calls request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IncomingCallsResult {
    /// List of incoming calls.
    pub calls: Vec<IncomingCall>,
}

/// An outgoing call (callee from the current item).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutgoingCall {
    /// The item being called.
    pub to: CallHierarchyItemResult,
    /// Ranges where the call occurs.
    pub from_ranges: Vec<Range>,
}

/// Result of outgoing calls request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutgoingCallsResult {
    /// List of outgoing calls.
    pub calls: Vec<OutgoingCall>,
}

/// Result of server logs request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerLogsResult {
    /// List of log entries.
    pub logs: Vec<crate::bridge::notifications::LogEntry>,
}

/// Result of server messages request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerMessagesResult {
    /// List of server messages.
    pub messages: Vec<crate::bridge::notifications::ServerMessage>,
}

/// A single parameter in a signature.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignatureParameter {
    /// Label of the parameter.
    pub label: String,
    /// Optional documentation for the parameter.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub documentation: Option<String>,
}

/// A single signature overload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignatureInfo {
    /// Full label of the signature.
    pub label: String,
    /// Optional documentation for the signature.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub documentation: Option<String>,
    /// Parameters of the signature.
    pub parameters: Vec<SignatureParameter>,
}

/// Result of a signature help request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignatureHelpResult {
    /// Available signatures.
    pub signatures: Vec<SignatureInfo>,
    /// Index of the active signature.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_signature: Option<u32>,
    /// Index of the active parameter within the active signature.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_parameter: Option<u32>,
}

/// Result of a go-to-implementation or go-to-type-definition request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocationsResult {
    /// Stable protocol provider identity.
    pub provider: String,
    /// Relationship represented by the returned targets.
    pub kind: NavigationKind,
    /// Locations found.
    pub locations: Vec<Location>,
    /// Whether item or response budgets omitted target data.
    pub truncated: bool,
}

/// A single inlay hint entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InlayHintEntry {
    /// Position of the hint (1-based MCP).
    pub position: Position2D,
    /// Label text for the hint.
    pub label: String,
    /// Hint kind (1 = Type, 2 = Parameter).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<u8>,
    /// Whether to add a space before the hint.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub padding_left: Option<bool>,
    /// Whether to add a space after the hint.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub padding_right: Option<bool>,
    /// Tooltip text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tooltip: Option<String>,
}

/// Result of an inlay hints request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InlayHintsResult {
    /// List of inlay hints.
    pub hints: Vec<InlayHintEntry>,
}
