//! Public MCP-facing result/data-transfer types returned by the tool-call
//! handlers in the sibling domain modules.

use serde::{Deserialize, Serialize};

/// Opaque project-actor-owned reference to a symbol at one source snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[schemars(inline)]
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Position2D {
    /// Line number (1-based).
    pub line: u32,
    /// Character offset (1-based).
    pub character: u32,
}

/// Range in a document (1-based for MCP).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Range {
    /// Start position.
    pub start: Position2D,
    /// End position.
    pub end: Position2D,
}

/// Location in a document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum SourceContext {
    /// The source was safely resolved.
    Available(SourceFrame),
    /// The source exists but was omitted from the bounded response.
    Deferred {
        /// Direct snapshot-bound resource for the omitted source.
        resource: DeferredResourceReference,
    },
    /// The source could not be safely resolved.
    Unavailable {
        /// Stable machine-readable reason.
        reason: SourceUnavailableReason,
    },
}

/// A bounded, line-numbered source excerpt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SourceFrame {
    /// Canonical filesystem path.
    pub path: String,
    /// Canonical file URI.
    pub uri: String,
    /// Inclusive 1-based line window covered by `text`.
    pub range: Range,
    /// Normalized semantic result range highlighted within the source window.
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
    /// Direct resource for the complete selected context when this frame is bounded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource: Option<DeferredResourceReference>,
}

/// Snapshot-bound MCP resource for context omitted from an inline result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DeferredResourceReference {
    /// Resource URI for `resources/read` or the `read_semantic_resource` tool fallback.
    pub uri: String,
    /// Stable kind of deferred payload.
    pub kind: String,
    /// Snapshot content hash used to reject stale reads.
    pub snapshot_hash: String,
    /// Open-document version when available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub document_version: Option<i32>,
    /// Known byte size of the deferred payload.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_bytes: Option<usize>,
}

/// Stable reasons source text is unavailable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
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
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
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
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
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
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ReferencesResult {
    /// Stable protocol provider identity.
    pub provider: String,
    /// References grouped by project-relative file.
    pub groups: Vec<ReferenceGroup>,
    /// Declaration location, returned once and excluded from use groups.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub declaration: Option<Location>,
    /// Number of references reported by the language server.
    pub total_references: usize,
    /// Number of references returned after response budgets.
    pub returned_references: usize,
    /// Number of file/symbol groups before limits.
    pub total_groups: usize,
    /// Number of file/symbol groups returned.
    pub returned_groups: usize,
    /// Number of complete groups omitted by limits.
    pub omitted_groups: usize,
    /// Limits applied to this result.
    pub limits: SemanticResultLimits,
    /// Whether response budgets omitted source or references.
    pub truncated: bool,
    /// Cursor for the next deterministic page of compact references.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

/// References returned from one source file.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ReferenceGroup {
    /// Project-relative path, or the absolute path for external sources.
    pub project_relative_path: String,
    /// URI shared by every reference in this group.
    pub uri: String,
    /// File-backed path used internally to mint follow-up handles.
    #[serde(skip)]
    #[schemars(skip)]
    pub path: Option<String>,
    /// Enclosing symbol when the language server can determine one safely.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enclosing_symbol: Option<String>,
    /// References within the group, in source order.
    pub references: Vec<ReferenceUse>,
    /// Source snapshot and merged context shared by the references.
    pub source: ReferenceSource,
}

/// One source use of the referenced symbol.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ReferenceUse {
    /// `[start_line, start_character, end_line, end_character]`, all 1-based.
    pub range: [u32; 4],
    /// Semantic role, reported conservatively when unavailable from the server.
    pub role: ReferenceRole,
    /// Snapshot-bound target for coordinate-free semantic follow-ups.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol_handle: Option<SymbolHandle>,
    /// Per-reference snapshot used internally to mint the compact handle.
    #[serde(skip)]
    #[schemars(skip)]
    pub(crate) snapshot: Option<ReferenceSnapshot>,
}

/// Compact actor-only snapshot needed to mint a reference handle.
#[derive(Debug, Clone)]
pub struct ReferenceSnapshot {
    /// Canonical file path.
    pub path: String,
    /// Tracked document version when the source is open.
    pub document_version: Option<i32>,
    /// Content hash used when no document version is available.
    pub content_hash: String,
}

/// Source metadata shared by references from one file and enclosing symbol.
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ReferenceSource {
    /// SHA-256 of the source snapshot used for available chunks.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_hash: Option<String>,
    /// Open-document version used for available chunks.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub document_version: Option<i32>,
    /// Merged source windows, in source order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub chunks: Vec<ReferenceSourceChunk>,
    /// Snapshot-bound resources for context omitted by the response budget.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deferred: Vec<DeferredResourceReference>,
    /// Reasons context could not be resolved, deduplicated for this group.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unavailable: Vec<SourceUnavailableReason>,
}

/// One merged, line-numbered source window shared by nearby references.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ReferenceSourceChunk {
    /// Inclusive `[start_line, end_line]` covered by `text`.
    pub lines: [u32; 2],
    /// Line-numbered source text.
    pub text: String,
}

/// Safely known role of a reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReferenceRole {
    /// Definition or declaration, identified by a language-server definition result.
    Declaration,
    /// The language server did not provide enough information to classify it.
    Unknown,
}

/// Shared bounds for reference and call-hierarchy results.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SemanticResultLimits {
    /// Maximum references or call groups returned overall.
    #[serde(default = "default_semantic_total_limit")]
    pub total: usize,
    /// Maximum groups returned from one file.
    #[serde(default = "default_semantic_per_file_limit")]
    pub per_file: usize,
    /// Maximum references or call sites returned for one symbol.
    #[serde(default = "default_semantic_per_symbol_limit")]
    pub per_symbol: usize,
}

const fn default_semantic_total_limit() -> usize {
    200
}

const fn default_semantic_per_file_limit() -> usize {
    50
}

const fn default_semantic_per_symbol_limit() -> usize {
    25
}

impl Default for SemanticResultLimits {
    fn default() -> Self {
        Self {
            total: default_semantic_total_limit(),
            per_file: default_semantic_per_file_limit(),
            per_symbol: default_semantic_per_symbol_limit(),
        }
    }
}

/// Diagnostic severity.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, schemars::JsonSchema,
)]
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Diagnostic {
    /// Range where the diagnostic applies.
    pub range: Range,
    /// Severity of the diagnostic.
    pub severity: DiagnosticSeverity,
    /// Diagnostic message.
    pub message: String,
    /// Optional diagnostic code.
    pub code: Option<String>,
    /// Source, grouping, and fix-preview metadata for this diagnostic.
    #[serde(flatten)]
    pub context: DiagnosticContext,
}

/// Model-ready context attached to a diagnostic.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DiagnosticContext {
    /// Canonical filesystem path when the diagnostic is file-backed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Path relative to the owning project root.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_relative_path: Option<String>,
    /// Canonical document URI.
    pub uri: String,
    /// Bounded source surrounding the highlighted range.
    pub source_frame: SourceContext,
    /// Language-server subsystem that produced the diagnostic.
    #[serde(rename = "source", skip_serializing_if = "Option::is_none")]
    pub diagnostic_source: Option<String>,
    /// Documentation URI associated with the diagnostic code.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code_description: Option<String>,
    /// Standard LSP diagnostic tags.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Related diagnostics with their own authorized source frames.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub related_information: Vec<DiagnosticRelatedInformation>,
    /// Language-server-specific payload after secret redaction.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
    /// Number of identical diagnostics represented by this group.
    pub occurrence_count: usize,
    /// Actor-owned code-action handles accepted by the preview flow.
    pub fix_handles: Vec<String>,
    /// Exact additional locations represented by this group when requested.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub occurrences: Vec<Location>,
}

impl Default for DiagnosticContext {
    fn default() -> Self {
        Self {
            path: None,
            project_relative_path: None,
            uri: String::new(),
            source_frame: SourceContext::Unavailable {
                reason: SourceUnavailableReason::NotFound,
            },
            diagnostic_source: None,
            code_description: None,
            tags: Vec::new(),
            related_information: Vec::new(),
            data: None,
            occurrence_count: 1,
            fix_handles: Vec::new(),
            occurrences: Vec::new(),
        }
    }
}

/// A related diagnostic location and its bounded source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DiagnosticRelatedInformation {
    /// Related source location.
    pub location: Location,
    /// Relationship message supplied by the language server.
    pub message: String,
}

/// Result of a diagnostics request.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DiagnosticsResult {
    /// List of diagnostics for the document.
    pub diagnostics: Vec<Diagnostic>,
    /// Diagnostics before filtering and grouping.
    pub total_diagnostics: usize,
    /// Diagnostic occurrences represented in the response.
    pub returned_diagnostics: usize,
    /// Stable groups before response limits.
    pub total_groups: usize,
    /// Stable groups returned.
    pub returned_groups: usize,
    /// Complete groups omitted by response limits.
    pub omitted_groups: usize,
    /// Whether filtering or response budgets shortened the result.
    pub truncated: bool,
    /// Filters and bounds applied to this result.
    pub filters: DiagnosticOptions,
}

/// Filters and explicit response bounds for diagnostics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DiagnosticOptions {
    /// Severities to retain; empty keeps every severity.
    #[serde(default)]
    pub severities: Vec<DiagnosticSeverity>,
    /// LSP source names to retain; empty keeps every source.
    #[serde(default)]
    pub sources: Vec<String>,
    /// Diagnostic codes to retain; empty keeps every code.
    #[serde(default)]
    pub codes: Vec<String>,
    /// Include diagnostics explicitly identified as inactive code.
    #[serde(default = "default_true")]
    pub include_inactive: bool,
    /// Include diagnostics under generated output paths.
    #[serde(default = "default_true")]
    pub include_generated: bool,
    /// Preserve every exact location within collapsed groups.
    #[serde(default)]
    pub preserve_locations: bool,
    /// Maximum diagnostic groups returned.
    #[serde(default = "default_diagnostic_item_limit")]
    pub item_limit: usize,
    /// Maximum source-frame bytes across the response.
    #[serde(default = "default_diagnostic_byte_limit")]
    pub byte_limit: usize,
}

const fn default_true() -> bool {
    true
}

const fn default_diagnostic_item_limit() -> usize {
    100
}

const fn default_diagnostic_byte_limit() -> usize {
    32 * 1024
}

impl Default for DiagnosticOptions {
    fn default() -> Self {
        Self {
            severities: Vec::new(),
            sources: Vec::new(),
            codes: Vec::new(),
            include_inactive: true,
            include_generated: true,
            preserve_locations: false,
            item_limit: default_diagnostic_item_limit(),
            byte_limit: default_diagnostic_byte_limit(),
        }
    }
}

impl DiagnosticsResult {
    /// Build an ungrouped result for internal pull/push merging.
    pub(crate) fn raw(diagnostics: Vec<Diagnostic>) -> Self {
        let count = diagnostics.len();
        Self {
            diagnostics,
            total_diagnostics: count,
            returned_diagnostics: count,
            total_groups: count,
            returned_groups: count,
            omitted_groups: 0,
            truncated: false,
            filters: DiagnosticOptions::default(),
        }
    }
}

/// A text edit operation.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TextEdit {
    /// Range to replace.
    pub range: Range,
    /// New text.
    pub new_text: String,
}

/// Changes to a document.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DocumentChanges {
    /// URI of the document.
    pub uri: String,
    /// List of edits to apply.
    pub edits: Vec<TextEdit>,
}

/// Result of a rename request.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RenameResult {
    /// Changes to apply across documents.
    pub changes: Vec<DocumentChanges>,
}

/// A completion item.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
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
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CompletionsResult {
    /// List of completion items.
    pub items: Vec<Completion>,
}

/// A document symbol.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
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
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
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
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct FormatDocumentResult {
    /// List of edits to format the document.
    pub edits: Vec<TextEdit>,
}

/// A workspace symbol.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
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
    /// Whether the symbol is under a generated or build-output directory.
    #[serde(default)]
    pub is_generated: bool,
}

/// Requested workspace-symbol name matching behavior.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[schemars(inline)]
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
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, schemars::JsonSchema,
)]
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
#[schemars(inline)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceSymbolScope {
    /// Only symbols under a registered project root.
    #[default]
    Project,
    /// Include dependency and other external symbols returned by the language server.
    All,
}

/// Workspace-symbol ownership relative to the registered project.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceSymbolOrigin {
    /// Symbol source is under a registered project root.
    ProjectLocal,
    /// Symbol source is outside every registered project root.
    External,
}

/// Result of workspace symbol search.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
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

/// Selectable sections of a high-level symbol inspection bundle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[schemars(inline)]
#[serde(rename_all = "snake_case")]
pub enum InspectSymbolSectionKind {
    /// Bounded declaration or body source.
    Declaration,
    /// Signature, type, and documentation.
    Hover,
    /// Definition targets.
    Definitions,
    /// Trait, interface, or symbol implementations.
    Implementations,
    /// Grouped references and call sites.
    References,
    /// Incoming and outgoing call hierarchy samples.
    Calls,
    /// Related tests, discovered but never executed.
    Tests,
    /// Runnable metadata, discovered but never executed.
    Runnables,
    /// Diagnostics intersecting the selected symbol.
    Diagnostics,
}

/// Actor-owned inputs for resolving and inspecting one project symbol.
#[derive(Debug, Clone)]
#[allow(missing_docs)]
pub struct InspectSymbolRequest {
    pub symbol_handle: Option<SymbolHandle>,
    pub query: Option<String>,
    pub kind: Option<String>,
    pub path: Option<String>,
    pub container: Option<String>,
    pub candidate_limit: u32,
    pub sections: Vec<InspectSymbolSectionKind>,
    pub budget: InspectSymbolBudget,
}

impl InspectSymbolRequest {
    /// Return whether the caller selected a section, applying defaults for an empty list.
    #[must_use]
    pub fn wants(&self, section: InspectSymbolSectionKind) -> bool {
        if self.sections.is_empty() {
            matches!(
                section,
                InspectSymbolSectionKind::Declaration
                    | InspectSymbolSectionKind::Implementations
                    | InspectSymbolSectionKind::References
                    | InspectSymbolSectionKind::Tests
                    | InspectSymbolSectionKind::Diagnostics
            )
        } else {
            self.sections.contains(&section)
        }
    }
}

/// Cross-section bounds for a symbol inspection response.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, schemars::JsonSchema)]
#[schemars(inline)]
pub struct InspectSymbolBudget {
    /// Maximum serialized response bytes.
    #[serde(default = "default_inspect_bytes")]
    pub max_bytes: usize,
    /// Maximum items requested from each collection-producing provider.
    #[serde(default = "default_inspect_items")]
    pub max_items: usize,
}

impl Default for InspectSymbolBudget {
    fn default() -> Self {
        Self {
            max_bytes: default_inspect_bytes(),
            max_items: default_inspect_items(),
        }
    }
}

const fn default_inspect_bytes() -> usize {
    64 * 1024
}

const fn default_inspect_items() -> usize {
    20
}

/// Completeness of one requested inspection section.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum InspectSectionCompleteness {
    /// Provider returned every requested item within bounds.
    Complete,
    /// Provider data is available but a response bound omitted content.
    Partial,
    /// Provider data was retained behind a direct resource because the bundle budget was full.
    Deferred,
    /// Provider does not advertise the requested capability.
    Unsupported,
    /// Capability may exist but the provider request failed.
    Unavailable,
    /// Caller did not select this section.
    NotRequested,
}

/// One typed inspection section with uniform provenance and bounds metadata.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct InspectSection<T> {
    /// Stable provider identity, when a provider was selected.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Machine-readable availability and completeness state.
    pub completeness: InspectSectionCompleteness,
    /// Items available before bounds.
    #[serde(skip_serializing_if = "is_zero")]
    pub total: usize,
    /// Items represented in `data`.
    #[serde(skip_serializing_if = "is_zero")]
    pub returned: usize,
    /// Whether bounds omitted items or source.
    #[serde(skip_serializing_if = "is_false")]
    pub truncated: bool,
    /// Actionable unsupported or unavailable reason.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Direct resource for data omitted from the inline bundle.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource: Option<DeferredResourceReference>,
    /// Typed section payload when available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<T>,
}

impl<T> InspectSection<T> {
    fn is_not_requested(&self) -> bool {
        self.completeness == InspectSectionCompleteness::NotRequested
    }

    /// Build metadata for a section omitted by caller selection.
    #[must_use]
    pub const fn not_requested() -> Self {
        Self {
            provider: None,
            completeness: InspectSectionCompleteness::NotRequested,
            total: 0,
            returned: 0,
            truncated: false,
            reason: None,
            resource: None,
            data: None,
        }
    }

    /// Build metadata for a provider failure.
    #[must_use]
    pub fn unavailable(reason: impl Into<String>) -> Self {
        Self {
            provider: None,
            completeness: InspectSectionCompleteness::Unavailable,
            total: 0,
            returned: 0,
            truncated: false,
            reason: Some(reason.into()),
            resource: None,
            data: None,
        }
    }

    /// Build metadata for a provider without the requested capability.
    #[must_use]
    pub fn unsupported(provider: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            provider: Some(provider.into()),
            completeness: InspectSectionCompleteness::Unsupported,
            total: 0,
            returned: 0,
            truncated: false,
            reason: Some(reason.into()),
            resource: None,
            data: None,
        }
    }

    /// Build an available typed section and derive completeness from truncation.
    #[must_use]
    pub fn available(
        provider: impl Into<String>,
        total: usize,
        returned: usize,
        truncated: bool,
        data: T,
    ) -> Self {
        Self {
            provider: Some(provider.into()),
            completeness: if truncated {
                InspectSectionCompleteness::Partial
            } else {
                InspectSectionCompleteness::Complete
            },
            total,
            returned,
            truncated,
            reason: None,
            resource: None,
            data: Some(data),
        }
    }

    /// Build section metadata for data retained behind a deferred resource.
    #[must_use]
    pub fn deferred(
        provider: impl Into<String>,
        total: usize,
        returned: usize,
        reason: impl Into<String>,
        resource: DeferredResourceReference,
    ) -> Self {
        Self {
            provider: Some(provider.into()),
            completeness: InspectSectionCompleteness::Deferred,
            total,
            returned,
            truncated: true,
            reason: Some(reason.into()),
            resource: Some(resource),
            data: None,
        }
    }
}

impl<T> Default for InspectSection<T> {
    fn default() -> Self {
        Self::not_requested()
    }
}

/// Incoming and outgoing call samples returned together.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct InspectCalls {
    /// Bounded callers and call sites.
    pub incoming: IncomingCallsResult,
    /// Bounded callees and call sites.
    pub outgoing: OutgoingCallsResult,
}

/// Exact resolution outcome for a symbol inspection request.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum InspectSymbolResolution {
    /// One exact symbol was selected safely.
    Selected {
        /// Ranked source-bearing symbol metadata for query resolution.
        symbol: Option<Box<WorkspaceSymbol>>,
        /// Snapshot-bound handle safe for semantic follow-ups.
        symbol_handle: Option<SymbolHandle>,
    },
    /// Several exact candidates remain; clients must disambiguate.
    Ambiguous {
        /// Ranked source-bearing candidates; never silently choose one.
        candidates: Vec<WorkspaceSymbol>,
    },
    /// A previously valid handle no longer matches its source snapshot.
    ///
    /// This is an expected refresh condition, not a malformed request. The
    /// caller should rerun symbol discovery and use the replacement handle.
    Stale {
        /// Handle that must be refreshed.
        symbol_handle: SymbolHandle,
        /// Human-readable reason for the refresh.
        reason: String,
        /// Whether retrying after discovery is safe.
        retryable: bool,
    },
    /// No exact candidate matched the supplied constraints.
    NotFound,
}

/// Typed semantic sections returned for one selected symbol.
#[derive(Debug, Clone, Default, Serialize, schemars::JsonSchema)]
pub struct InspectSymbolSections {
    /// Bounded declaration or body source.
    #[serde(skip_serializing_if = "InspectSection::is_not_requested")]
    pub declaration: InspectSection<SourceContext>,
    /// Signature, type, and documentation at the symbol.
    #[serde(skip_serializing_if = "InspectSection::is_not_requested")]
    pub hover: InspectSection<HoverResult>,
    /// Definition targets with bounded source and handles.
    #[serde(skip_serializing_if = "InspectSection::is_not_requested")]
    pub definitions: InspectSection<DefinitionResult>,
    /// Implementation targets with bounded source and handles.
    #[serde(skip_serializing_if = "InspectSection::is_not_requested")]
    pub implementations: InspectSection<LocationsResult>,
    /// Grouped references with bounded call-site source.
    #[serde(skip_serializing_if = "InspectSection::is_not_requested")]
    pub references: InspectSection<ReferencesResult>,
    /// Incoming and outgoing call samples.
    #[serde(skip_serializing_if = "InspectSection::is_not_requested")]
    pub calls: InspectSection<InspectCalls>,
    /// Related tests discovered without executing them.
    #[serde(skip_serializing_if = "InspectSection::is_not_requested")]
    pub tests: InspectSection<crate::bridge::SemanticDiscoveryResult>,
    /// Runnable metadata discovered without executing commands.
    #[serde(skip_serializing_if = "InspectSection::is_not_requested")]
    pub runnables: InspectSection<crate::bridge::SemanticDiscoveryResult>,
    /// Diagnostics intersecting the selected symbol range.
    #[serde(skip_serializing_if = "InspectSection::is_not_requested")]
    pub diagnostics: InspectSection<DiagnosticsResult>,
}

#[allow(clippy::trivially_copy_pass_by_ref)]
const fn is_zero(value: &usize) -> bool {
    *value == 0
}

#[allow(clippy::trivially_copy_pass_by_ref)]
const fn is_false(value: &bool) -> bool {
    !*value
}

/// Bounded, snapshot-coherent answer about one project symbol.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct InspectSymbolResult {
    /// Exact, ambiguous, or missing resolution outcome.
    pub resolution: InspectSymbolResolution,
    /// Requested typed semantic sections and uniform metadata.
    pub sections: InspectSymbolSections,
    /// Bounds applied to the response.
    pub budget: InspectSymbolBudget,
    /// Serialized bytes in this bundle before the MCP envelope.
    pub returned_bytes: usize,
    /// Whether the total budget removed lower-priority sections or candidates.
    pub truncated: bool,
}

/// A single code action.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
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
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkspaceEditDescription {
    /// Changes to apply to documents.
    pub changes: Vec<DocumentChanges>,
}

/// Description of a command.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
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
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CodeActionsResult {
    /// Available code actions.
    pub actions: Vec<CodeAction>,
}

/// A call hierarchy item.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<SourceContext>,
    /// Snapshot-bound target for coordinate-free semantic follow-ups.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbol_handle: Option<SymbolHandle>,
    /// Opaque data to pass to incoming/outgoing calls.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

/// Result of call hierarchy prepare request.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CallHierarchyPrepareResult {
    /// Stable protocol provider identity.
    pub provider: String,
    /// Relationship represented by the returned items.
    pub kind: NavigationKind,
    /// List of callable items at the position.
    pub items: Vec<CallHierarchyItemResult>,
    /// Total items in the prepared snapshot.
    pub total_items: usize,
    /// Items returned in this page.
    pub returned_items: usize,
    /// Cursor for the next deterministic page.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    /// Whether item or response budgets omitted target data.
    pub truncated: bool,
}

/// An incoming call (caller of the current item).
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct IncomingCall {
    /// The item that calls the current item.
    pub from: CallHierarchyItemResult,
    /// Ranges where the call occurs.
    pub call_sites: Vec<CallSite>,
}

/// One bounded call site in a caller's source.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CallSite {
    /// Highlighted call expression range.
    pub range: Range,
    /// Bounded source surrounding the call.
    pub source: SourceContext,
}

/// Result of incoming calls request.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct IncomingCallsResult {
    /// Stable protocol provider identity.
    pub provider: String,
    /// List of incoming calls.
    pub calls: Vec<IncomingCall>,
    /// Complete call-group count before limits.
    pub total_calls: usize,
    /// Returned call-group count.
    pub returned_calls: usize,
    /// Complete call-site count before limits.
    pub total_call_sites: usize,
    /// Returned call-site count.
    pub returned_call_sites: usize,
    /// Groups omitted by limits.
    pub omitted_groups: usize,
    /// Whether any item or source budget truncated the result.
    pub truncated: bool,
    /// Limits applied to this result.
    pub limits: SemanticResultLimits,
}

/// An outgoing call (callee from the current item).
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct OutgoingCall {
    /// The item being called.
    pub to: CallHierarchyItemResult,
    /// Ranges where the call occurs.
    pub call_sites: Vec<CallSite>,
}

/// Result of outgoing calls request.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct OutgoingCallsResult {
    /// Stable protocol provider identity.
    pub provider: String,
    /// List of outgoing calls.
    pub calls: Vec<OutgoingCall>,
    /// Complete call-group count before limits.
    pub total_calls: usize,
    /// Returned call-group count.
    pub returned_calls: usize,
    /// Complete call-site count before limits.
    pub total_call_sites: usize,
    /// Returned call-site count.
    pub returned_call_sites: usize,
    /// Groups omitted by limits.
    pub omitted_groups: usize,
    /// Whether any item or source budget truncated the result.
    pub truncated: bool,
    /// Limits applied to this result.
    pub limits: SemanticResultLimits,
}

/// Result of server logs request.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ServerLogsResult {
    /// List of log entries.
    pub logs: Vec<crate::bridge::notifications::LogEntry>,
}

/// Result of server messages request.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ServerMessagesResult {
    /// List of server messages.
    pub messages: Vec<crate::bridge::notifications::ServerMessage>,
}

/// A single parameter in a signature.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SignatureParameter {
    /// Label of the parameter.
    pub label: String,
    /// Optional documentation for the parameter.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub documentation: Option<String>,
}

/// A single signature overload.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
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
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
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
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
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
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
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
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct InlayHintsResult {
    /// List of inlay hints.
    pub hints: Vec<InlayHintEntry>,
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::{
        InspectSection, InspectSymbolRequest, InspectSymbolSectionKind, InspectSymbolSections,
    };

    fn request_with_sections(sections: Vec<InspectSymbolSectionKind>) -> InspectSymbolRequest {
        InspectSymbolRequest {
            symbol_handle: None,
            query: Some("symbol".to_owned()),
            kind: None,
            path: None,
            container: None,
            candidate_limit: 10,
            sections,
            budget: super::InspectSymbolBudget::default(),
        }
    }

    #[test]
    fn empty_section_selection_uses_the_bounded_default_bundle() {
        let request = request_with_sections(Vec::new());
        for section in [
            InspectSymbolSectionKind::Declaration,
            InspectSymbolSectionKind::Implementations,
            InspectSymbolSectionKind::References,
            InspectSymbolSectionKind::Tests,
            InspectSymbolSectionKind::Diagnostics,
        ] {
            assert!(request.wants(section));
        }
        for section in [
            InspectSymbolSectionKind::Hover,
            InspectSymbolSectionKind::Definitions,
            InspectSymbolSectionKind::Calls,
            InspectSymbolSectionKind::Runnables,
        ] {
            assert!(!request.wants(section));
        }
    }

    #[test]
    fn explicit_section_selection_does_not_expand_to_defaults() {
        let request = request_with_sections(vec![InspectSymbolSectionKind::Calls]);
        assert!(request.wants(InspectSymbolSectionKind::Calls));
        assert!(!request.wants(InspectSymbolSectionKind::References));
    }

    #[test]
    fn unrequested_inspect_sections_and_empty_metadata_are_not_serialized() {
        let sections = InspectSymbolSections {
            declaration: InspectSection::unsupported("standard_lsp", "not supported"),
            ..InspectSymbolSections::default()
        };

        let value = serde_json::to_value(sections).unwrap();

        assert_eq!(value.as_object().unwrap().len(), 1);
        assert_eq!(value["declaration"]["completeness"], "unsupported");
        assert_eq!(value["declaration"]["provider"], "standard_lsp");
        assert!(value["declaration"].get("total").is_none());
        assert!(value["declaration"].get("returned").is_none());
        assert!(value["declaration"].get("truncated").is_none());
        assert_eq!(value["declaration"]["reason"], "not supported");
        assert!(value["declaration"].get("data").is_none());
    }
}
