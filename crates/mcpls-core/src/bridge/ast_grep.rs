//! In-process AST-grep fallback for workspace symbol lookup.

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use ast_grep_core::replacer::TemplateFix;
use ast_grep_core::tree_sitter::{LanguageExt, StrDoc};
use ast_grep_core::{Node, Pattern};
use ast_grep_language::{Language, SupportLang};
use ignore::WalkBuilder;

use super::encoding::{EncodingConverter, PositionEncoding};
use super::state::path_to_uri;

const MAX_SCANNED_FILES: usize = 4_096;
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_TOTAL_BYTES: u64 = 64 * 1024 * 1024;
const MAX_SCAN_DURATION: Duration = Duration::from_secs(10);
const MAX_MATCHES: usize = 4_096;
const MAX_AFFECTED_FILES: usize = 64;
const MAX_PLANNED_FILE_BYTES: usize = 8 * 1024 * 1024;
const MAX_PLANNED_TOTAL_BYTES: usize = 16 * 1024 * 1024;
const MAX_QUERY_BYTES: usize = 64 * 1024;
const MAX_REPLACEMENT_BYTES: usize = 64 * 1024;
const GENERATED_DIRECTORIES: &[&str] = &[
    ".direnv",
    "build",
    "dist",
    "generated",
    "node_modules",
    "target",
];

#[derive(Debug, Clone)]
pub struct Symbol {
    pub(crate) name: String,
    pub(crate) kind: String,
    pub(crate) path: PathBuf,
    pub(crate) start_line: u32,
    pub(crate) start_character: u32,
    pub(crate) end_line: u32,
    pub(crate) end_character: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructuralMatch {
    pub path: PathBuf,
    pub range: lsp_types::Range,
}

#[derive(Debug, Clone)]
pub struct StructuralSearchResult {
    pub edit: Option<lsp_types::WorkspaceEdit>,
    pub matches: Vec<StructuralMatch>,
}

/// Search or replace an explicit ast-grep pattern without touching the filesystem.
pub async fn structural_search(
    root: PathBuf,
    language: String,
    query: String,
    replacement: Option<String>,
    encoding: PositionEncoding,
    source_overrides: HashMap<PathBuf, String>,
    parse_only: bool,
) -> Result<StructuralSearchResult, String> {
    let cancelled = Arc::new(AtomicBool::new(false));
    let worker_cancelled = Arc::clone(&cancelled);
    let cancellation_guard = CancellationGuard(cancelled);
    let result = tokio::task::spawn_blocking(move || {
        structural_search_sync(
            &root,
            &language,
            &query,
            replacement.as_deref(),
            encoding,
            &source_overrides,
            parse_only,
            &worker_cancelled,
        )
    })
    .await
    .map_err(|error| format!("ast-grep worker failed: {error}"))?;
    drop(cancellation_guard);
    result
}

#[allow(
    clippy::mutable_key_type,
    clippy::too_many_arguments,
    clippy::too_many_lines
)]
fn structural_search_sync(
    root: &std::path::Path,
    language: &str,
    query: &str,
    replacement: Option<&str>,
    encoding: PositionEncoding,
    source_overrides: &HashMap<PathBuf, String>,
    parse_only: bool,
    cancelled: &AtomicBool,
) -> Result<StructuralSearchResult, String> {
    if cancelled.load(Ordering::Relaxed) {
        return Err("ast-grep search cancelled".to_string());
    }
    if query.len() > MAX_QUERY_BYTES {
        return Err("ast-grep query exceeded byte limit".to_string());
    }
    if replacement.is_some_and(|replacement| replacement.len() > MAX_REPLACEMENT_BYTES) {
        return Err("ast-grep replacement exceeded template byte limit".to_string());
    }
    let language = ast_grep_language(language)
        .ok_or_else(|| format!("unsupported ast-grep language: {language}"))?;
    let pattern = Pattern::try_new(query, language)
        .map_err(|error| format!("invalid ast-grep pattern: {error}"))?;
    if pattern.has_error() {
        return Err("invalid ast-grep pattern: pattern contains a parse error".to_string());
    }
    let replacer = replacement
        .map(|replacement| TemplateFix::try_new(replacement, &language))
        .transpose()
        .map_err(|error| format!("invalid ast-grep replacement: {error}"))?;
    if let Some(replacer) = &replacer {
        let defined = pattern.defined_vars();
        let mut undefined = replacer
            .used_vars()
            .into_iter()
            .filter(|name| !defined.contains(name))
            .collect::<Vec<_>>();
        undefined.sort_unstable();
        if !undefined.is_empty() {
            return Err(format!(
                "ast-grep replacement uses undefined metavariables: {}",
                undefined.join(", ")
            ));
        }
    }
    if parse_only {
        return Ok(StructuralSearchResult {
            edit: None,
            matches: Vec::new(),
        });
    }

    let root = fs::canonicalize(root)
        .map_err(|error| format!("failed to canonicalize ast-grep root: {error}"))?;
    let started = Instant::now();
    let mut paths = BTreeSet::new();
    for entry in WalkBuilder::new(&root)
        .standard_filters(true)
        .filter_entry(|entry| !is_generated_path(entry.path()))
        .build()
        .flatten()
    {
        if cancelled.load(Ordering::Relaxed) {
            return Err("ast-grep search cancelled".to_string());
        }
        if started.elapsed() >= MAX_SCAN_DURATION {
            return Err("ast-grep search exceeded duration limit".to_string());
        }
        if paths.len() >= MAX_SCANNED_FILES {
            return Err("ast-grep search exceeded file limit".to_string());
        }
        if !entry
            .file_type()
            .is_some_and(|file_type| file_type.is_file())
        {
            continue;
        }
        let Ok(path) = fs::canonicalize(entry.path()) else {
            continue;
        };
        if SupportLang::from_path(&path) == Some(language) {
            paths.insert(path);
        }
    }

    let mut matches = Vec::new();
    let mut changes = HashMap::new();
    let mut affected_files = 0usize;
    let mut total_bytes = 0u64;
    let mut planned_total_bytes = 0usize;
    for path in paths {
        if cancelled.load(Ordering::Relaxed) {
            return Err("ast-grep search cancelled".to_string());
        }
        if started.elapsed() >= MAX_SCAN_DURATION {
            return Err("ast-grep search exceeded duration limit".to_string());
        }
        let source = source_overrides.get(&path).cloned().map_or_else(
            || fs::read_to_string(&path).map_err(|error| error.to_string()),
            Ok,
        )?;
        let source_bytes = u64::try_from(source.len())
            .map_err(|_| "ast-grep source size does not fit u64".to_string())?;
        if source_bytes > MAX_FILE_BYTES {
            continue;
        }
        total_bytes = total_bytes.saturating_add(source_bytes);
        if total_bytes > MAX_TOTAL_BYTES {
            return Err("ast-grep search exceeded byte limit".to_string());
        }

        let tree = language.ast_grep(&source);
        let ranges = tree
            .root()
            .find_all(&pattern)
            .map(|matched| matched.range())
            .collect::<Vec<_>>();
        if ranges.is_empty() {
            continue;
        }
        if matches.len().saturating_add(ranges.len()) > MAX_MATCHES {
            return Err("ast-grep search exceeded match limit".to_string());
        }
        affected_files += 1;
        if affected_files > MAX_AFFECTED_FILES {
            return Err("ast-grep search exceeded affected-file limit".to_string());
        }

        let mut previous_end = 0usize;
        for range in &ranges {
            if range.start < previous_end || range.end > source.len() {
                return Err("ast-grep produced overlapping or invalid matches".to_string());
            }
            let Some(start) = byte_offset_to_position(&source, range.start, encoding) else {
                return Err("ast-grep match start is not a valid text position".to_string());
            };
            let Some(end_position) = byte_offset_to_position(&source, range.end, encoding) else {
                return Err("ast-grep match end is not a valid text position".to_string());
            };
            matches.push(StructuralMatch {
                path: path.clone(),
                range: lsp_types::Range {
                    start,
                    end: end_position,
                },
            });
            previous_end = range.end;
        }

        let Some(replacer) = &replacer else {
            continue;
        };
        if replacement.is_some_and(|replacement| {
            replacement.len().saturating_mul(ranges.len()) > MAX_PLANNED_FILE_BYTES
        }) {
            return Err("ast-grep replacement exceeded expansion byte limit".to_string());
        }
        let replacement_edits = tree.root().replace_all(&pattern, replacer);
        if replacement_edits.len() != ranges.len() {
            return Err("ast-grep replacement produced ambiguous matches".to_string());
        }
        let mut planned = source.clone();
        for edit in replacement_edits.into_iter().rev() {
            let end = edit.position + edit.deleted_length;
            if !planned.is_char_boundary(edit.position) || !planned.is_char_boundary(end) {
                return Err("ast-grep replacement is not on a UTF-8 boundary".to_string());
            }
            let inserted = String::from_utf8(edit.inserted_text)
                .map_err(|_| "ast-grep replacement is not valid UTF-8".to_string())?;
            planned.replace_range(edit.position..end, &inserted);
        }
        let planned_tree = language.ast_grep(&planned);
        if planned_tree
            .root()
            .dfs()
            .any(|node| node.is_error() || node.is_missing())
        {
            return Err("ast-grep replacement produced invalid syntax".to_string());
        }
        if planned.len() > MAX_PLANNED_FILE_BYTES {
            return Err("ast-grep replacement exceeded per-file byte limit".to_string());
        }
        planned_total_bytes = planned_total_bytes.saturating_add(source.len() + planned.len());
        if planned_total_bytes > MAX_PLANNED_TOTAL_BYTES {
            return Err("ast-grep replacement exceeded total byte limit".to_string());
        }
        let end = byte_offset_to_position(&source, source.len(), encoding)
            .ok_or_else(|| "ast-grep source end is not a valid text position".to_string())?;
        changes.insert(
            path_to_uri(&path).map_err(|error| error.to_string())?,
            vec![lsp_types::TextEdit {
                range: lsp_types::Range {
                    start: lsp_types::Position::new(0, 0),
                    end,
                },
                new_text: planned,
            }],
        );
    }

    Ok(StructuralSearchResult {
        edit: replacer.map(|_| lsp_types::WorkspaceEdit {
            changes: Some(changes),
            document_changes: None,
            change_annotations: None,
        }),
        matches,
    })
}

/// Search configured workspace roots with ast-grep's Rust library.
///
/// Parsing is isolated on a blocking worker because tree-sitter parsing and
/// filesystem traversal are synchronous. A failed read or parse only removes
/// that file from the degraded lookup result.
pub async fn search(
    roots: &[PathBuf],
    languages: &[String],
    query: &str,
    kind_filter: Option<&str>,
    limit: usize,
) -> Vec<Symbol> {
    if limit == 0 {
        return Vec::new();
    }

    let roots = roots.to_vec();
    let languages = languages.to_vec();
    let query = query.to_string();
    let kind_filter = kind_filter.map(str::to_owned);
    let cancelled = Arc::new(AtomicBool::new(false));
    let worker_cancelled = Arc::clone(&cancelled);
    let cancellation_guard = CancellationGuard(cancelled);
    let result = tokio::task::spawn_blocking(move || {
        search_sync(
            &roots,
            &languages,
            &query,
            kind_filter.as_deref(),
            limit,
            &worker_cancelled,
        )
    })
    .await
    .unwrap_or_default();
    drop(cancellation_guard);
    result
}

struct CancellationGuard(Arc<AtomicBool>);

impl Drop for CancellationGuard {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

#[allow(clippy::too_many_lines)]
fn search_sync(
    roots: &[PathBuf],
    languages: &[String],
    query: &str,
    kind_filter: Option<&str>,
    limit: usize,
    cancelled: &AtomicBool,
) -> Vec<Symbol> {
    if cancelled.load(Ordering::Relaxed) {
        return Vec::new();
    }
    let languages = languages
        .iter()
        .filter_map(|language| ast_grep_language(language))
        .collect::<Vec<_>>();
    if languages.is_empty() {
        return Vec::new();
    }

    let query = query.to_ascii_lowercase();
    let started = Instant::now();
    let mut paths = BTreeSet::new();
    for root in roots {
        for entry in WalkBuilder::new(root)
            .standard_filters(true)
            .filter_entry(|entry| !is_generated_path(entry.path()))
            .build()
            .flatten()
        {
            if cancelled.load(Ordering::Relaxed)
                || started.elapsed() >= MAX_SCAN_DURATION
                || paths.len() >= MAX_SCANNED_FILES
            {
                break;
            }
            if !entry
                .file_type()
                .is_some_and(|file_type| file_type.is_file())
            {
                continue;
            }
            let Ok(path) = fs::canonicalize(entry.path()) else {
                continue;
            };
            if languages
                .iter()
                .all(|language| SupportLang::from_path(&path) != Some(*language))
            {
                continue;
            }
            paths.insert(path);
        }
        if cancelled.load(Ordering::Relaxed)
            || started.elapsed() >= MAX_SCAN_DURATION
            || paths.len() >= MAX_SCANNED_FILES
        {
            break;
        }
    }

    let mut symbols = Vec::new();
    let mut total_bytes: u64 = 0;
    for path in paths {
        if cancelled.load(Ordering::Relaxed) || started.elapsed() >= MAX_SCAN_DURATION {
            break;
        }
        let Ok(metadata) = fs::metadata(&path) else {
            continue;
        };
        if metadata.len() > MAX_FILE_BYTES
            || total_bytes.saturating_add(metadata.len()) > MAX_TOTAL_BYTES
        {
            continue;
        }
        total_bytes = total_bytes.saturating_add(metadata.len());
        let Some(language) = languages
            .iter()
            .find(|language| SupportLang::from_path(&path) == Some(**language))
            .copied()
        else {
            continue;
        };
        let Ok(source) = fs::read_to_string(&path) else {
            continue;
        };
        if !contains_ascii_case_insensitive(&source, &query) {
            continue;
        }

        let tree = language.ast_grep(&source);
        for node in tree.root().dfs() {
            if cancelled.load(Ordering::Relaxed) || started.elapsed() >= MAX_SCAN_DURATION {
                return symbols;
            }
            let Some(kind) = symbol_kind(&node) else {
                continue;
            };
            if kind_filter.is_some_and(|filter| !kind.eq_ignore_ascii_case(filter)) {
                continue;
            }
            let Some(name) = symbol_name(&node) else {
                continue;
            };
            if !contains_ascii_case_insensitive(&name, &query) {
                continue;
            }
            let range = node.range();
            let Some((start_line, start_character)) =
                byte_offset_to_fallback_position(&source, range.start)
            else {
                continue;
            };
            let Some((end_line, end_character)) =
                byte_offset_to_fallback_position(&source, range.end)
            else {
                continue;
            };
            symbols.push(Symbol {
                name,
                kind: kind.to_string(),
                path: path.clone(),
                start_line,
                start_character,
                end_line,
                end_character,
            });
            if symbols.len() >= limit {
                return symbols;
            }
        }
    }
    symbols
}

fn contains_ascii_case_insensitive(haystack: &str, needle: &str) -> bool {
    needle.is_empty()
        || haystack
            .as_bytes()
            .windows(needle.len())
            .any(|candidate| candidate.eq_ignore_ascii_case(needle.as_bytes()))
}

#[cfg(feature = "bench")]
pub fn benchmark_workspace_symbol_count(
    root: &std::path::Path,
    query: &str,
    limit: usize,
) -> usize {
    search_sync(
        &[root.to_path_buf()],
        &["rust".to_string()],
        query,
        None,
        limit,
        &AtomicBool::new(false),
    )
    .len()
}

/// Convert tree-sitter byte offsets to the daemon's default UTF-8 MCP units.
/// Fallback results have no negotiated LSP encoding, so using the explicit
/// default keeps Unicode coordinates deterministic rather than relying on the
/// parser's point-column convention.
fn byte_offset_to_fallback_position(source: &str, offset: usize) -> Option<(u32, u32)> {
    let position = byte_offset_to_position(source, offset, PositionEncoding::Utf8)?;
    Some((position.line, position.character))
}

fn byte_offset_to_position(
    source: &str,
    offset: usize,
    encoding: PositionEncoding,
) -> Option<lsp_types::Position> {
    if offset > source.len() || !source.is_char_boundary(offset) {
        return None;
    }
    let line = source[..offset]
        .bytes()
        .filter(|byte| *byte == b'\n')
        .count();
    let line_start = source[..offset].rfind('\n').map_or(0, |index| index + 1);
    let character = EncodingConverter::new(encoding)
        .byte_offset_to_character(&source[line_start..offset], offset - line_start)
        .ok()?;
    Some(lsp_types::Position::new(
        u32::try_from(line).ok()?,
        character,
    ))
}

fn is_generated_path(path: &std::path::Path) -> bool {
    path.components().any(|component| {
        component
            .as_os_str()
            .to_str()
            .is_some_and(|name| GENERATED_DIRECTORIES.contains(&name))
    })
}

fn symbol_name<L: LanguageExt>(node: &Node<'_, StrDoc<L>>) -> Option<String> {
    ["name", "declarator", "left", "pattern"]
        .into_iter()
        .find_map(|field| node.field(field))
        .and_then(|name| identifier_text(&name))
        .or_else(|| node.children().find_map(|child| identifier_text(&child)))
}

fn identifier_text<L: LanguageExt>(node: &Node<'_, StrDoc<L>>) -> Option<String> {
    let kind = node.kind();
    let kind = kind.as_ref();
    let is_identifier = kind == "identifier"
        || kind == "type_identifier"
        || kind == "field_identifier"
        || kind.ends_with("_identifier")
        || kind.ends_with("_name");
    if is_identifier {
        let node_text = node.text();
        let text = node_text.trim();
        if !text.is_empty() && !text.contains(char::is_whitespace) {
            return Some(text.to_string());
        }
    }
    None
}

fn symbol_kind<L: LanguageExt>(node: &Node<'_, StrDoc<L>>) -> Option<&'static str> {
    let kind = node.kind();
    let kind = kind.to_ascii_lowercase();
    let kind = kind.as_str();
    if kind == "enum_variant" {
        Some("enum_member")
    } else if kind.contains("method")
        || (kind == "function_item" && has_ancestor_kind(node, "impl_item"))
    {
        Some("method")
    } else if kind.contains("function") || kind.contains("procedure") {
        Some("function")
    } else if kind.contains("class") {
        Some("class")
    } else if kind.contains("interface") || kind == "trait_item" {
        Some("interface")
    } else if kind.contains("enum") {
        Some("enum")
    } else if kind.contains("struct") || kind.contains("record") || kind.contains("union") {
        Some("struct")
    } else if kind.contains("module") || kind == "mod_item" || kind.contains("namespace") {
        Some("module")
    } else if kind.contains("constant") || kind == "const_item" || kind == "static_item" {
        Some("constant")
    } else if kind.contains("type_alias") || kind == "type_item" {
        Some("type")
    } else {
        None
    }
}

fn has_ancestor_kind<L: LanguageExt>(node: &Node<'_, StrDoc<L>>, target: &str) -> bool {
    let mut parent = node.parent();
    while let Some(candidate) = parent {
        if candidate.kind() == target {
            return true;
        }
        parent = candidate.parent();
    }
    false
}

fn ast_grep_language(language: &str) -> Option<SupportLang> {
    let lower = language.to_ascii_lowercase();
    let language = match lower.as_str() {
        "shellscript" | "sh" => "bash",
        "c++" => "cpp",
        "c#" => "csharp",
        "js" => "javascript",
        "ts" => "typescript",
        other => other,
    };
    language.parse().ok()
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::mutable_key_type)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn maps_supported_language_ids() {
        assert_eq!(ast_grep_language("rust"), Some(SupportLang::Rust));
        assert_eq!(ast_grep_language("shellscript"), Some(SupportLang::Bash));
        assert_eq!(ast_grep_language("nix"), Some(SupportLang::Nix));
        assert_eq!(ast_grep_language("unknown"), None);
    }

    #[test]
    fn extracts_rust_symbols_in_process() {
        let Ok(temp) = tempfile::tempdir() else {
            panic!("failed to create temporary directory");
        };
        let path = temp.path().join("fallback.rs");
        assert!(
            fs::write(&path, "struct AstFallback;\nfn fallback_function() {}\n").is_ok(),
            "failed to write test source"
        );

        let symbols = search_sync(
            &[temp.path().to_path_buf()],
            &["rust".to_string()],
            "fallback",
            None,
            10,
            &AtomicBool::new(false),
        );

        assert_eq!(symbols.len(), 2);
        assert_eq!(symbols[0].name, "AstFallback");
        assert_eq!(symbols[0].kind, "struct");
        assert_eq!(symbols[1].name, "fallback_function");
        assert_eq!(symbols[1].kind, "function");
    }

    #[test]
    fn text_prefilter_matches_symbol_query_case_insensitively() {
        assert!(contains_ascii_case_insensitive(
            "pub struct RootDatabase;",
            "rootdatabase"
        ));
        assert!(contains_ascii_case_insensitive(
            "fn überRoot() {}",
            "überroot"
        ));
        assert!(!contains_ascii_case_insensitive(
            "pub struct OtherDatabase;",
            "rootdatabase"
        ));
    }

    #[test]
    fn filters_kinds_before_the_result_limit_and_skips_generated_paths() {
        let Ok(temp) = tempfile::tempdir() else {
            panic!("failed to create temporary directory");
        };
        let target = temp.path().join("target");
        assert!(fs::create_dir_all(&target).is_ok());
        assert!(fs::write(target.join("generated.rs"), "struct TargetOnly;\n").is_ok());
        let path = temp.path().join("source.rs");
        assert!(fs::write(&path, "fn run() {}\nstruct RunStruct;\n").is_ok());

        let symbols = search_sync(
            &[temp.path().to_path_buf()],
            &["rust".to_string()],
            "run",
            Some("struct"),
            1,
            &AtomicBool::new(false),
        );

        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].name, "RunStruct");
        assert_eq!(symbols[0].kind, "struct");
    }

    #[test]
    fn classifies_rust_methods_and_enum_members() {
        let Ok(temp) = tempfile::tempdir() else {
            panic!("failed to create temporary directory");
        };
        let path = temp.path().join("kinds.rs");
        assert!(
            fs::write(
                &path,
                "struct Thing;\nimpl Thing { fn run(&self) {} }\nenum State { Running }\n"
            )
            .is_ok()
        );

        let symbols = search_sync(
            &[temp.path().to_path_buf()],
            &["rust".to_string()],
            "run",
            None,
            10,
            &AtomicBool::new(false),
        );
        assert!(
            symbols
                .iter()
                .any(|symbol| symbol.name == "run" && symbol.kind == "method")
        );

        let symbols = search_sync(
            &[temp.path().to_path_buf()],
            &["rust".to_string()],
            "running",
            None,
            10,
            &AtomicBool::new(false),
        );
        assert!(
            symbols
                .iter()
                .any(|symbol| symbol.name == "Running" && symbol.kind == "enum_member")
        );
    }

    #[test]
    fn canonicalizes_overlapping_roots_and_keeps_unicode_ranges_stable() {
        let Ok(temp) = tempfile::tempdir() else {
            panic!("failed to create temporary directory");
        };
        let path = temp.path().join("unicode.rs");
        let source = "const PREFIX: &str = \"😀\"; fn run() {}\n";
        assert!(fs::write(&path, source).is_ok());

        let symbols = search_sync(
            &[
                temp.path().to_path_buf(),
                temp.path().to_path_buf(),
                temp.path().join("."),
            ],
            &["rust".to_string()],
            "run",
            None,
            10,
            &AtomicBool::new(false),
        );

        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].start_line, 0);
        let Some(expected_start) = source
            .find("fn run")
            .and_then(|offset| u32::try_from(offset).ok())
        else {
            panic!("test source must contain the searched function");
        };
        assert_eq!(symbols[0].start_character, expected_start);
    }

    #[test]
    fn cancelled_scan_returns_without_traversing() {
        let Ok(temp) = tempfile::tempdir() else {
            panic!("failed to create temporary directory");
        };
        assert!(fs::write(temp.path().join("cancelled.rs"), "fn cancelled() {}\n").is_ok());
        let cancelled = AtomicBool::new(true);

        let symbols = search_sync(
            &[temp.path().to_path_buf()],
            &["rust".to_string()],
            "cancelled",
            None,
            10,
            &cancelled,
        );

        assert!(symbols.is_empty());
    }

    #[test]
    fn structural_search_reports_matches_without_building_an_edit() {
        let Ok(temp) = tempfile::tempdir() else {
            panic!("failed to create temporary directory");
        };
        assert!(
            fs::write(
                temp.path().join("search.rs"),
                "fn main() { foo(\"😀\"); foo(2); }\n"
            )
            .is_ok()
        );

        let result = structural_search_sync(
            temp.path(),
            "rust",
            "foo($A)",
            None,
            PositionEncoding::Utf16,
            &HashMap::new(),
            false,
            &AtomicBool::new(false),
        )
        .expect("structural search should succeed");

        assert!(result.edit.is_none());
        assert_eq!(result.matches.len(), 2);
        assert_eq!(result.matches[0].range.start.line, 0);
        assert_eq!(result.matches[0].range.start.character, 12);
    }

    #[test]
    fn structural_replacement_uses_dirty_source_and_builds_full_file_edit() {
        let Ok(temp) = tempfile::tempdir() else {
            panic!("failed to create temporary directory");
        };
        let path = temp.path().join("dirty.rs");
        assert!(fs::write(&path, "fn main() { foo(1); }\n").is_ok());
        let path = fs::canonicalize(path).expect("test path should canonicalize");
        let overrides = HashMap::from([(path.clone(), "fn main() { foo(2); }\n".to_string())]);

        let result = structural_search_sync(
            temp.path(),
            "rust",
            "foo($A)",
            Some("bar($A)"),
            PositionEncoding::Utf8,
            &overrides,
            false,
            &AtomicBool::new(false),
        )
        .expect("structural replacement should succeed");

        assert_eq!(result.matches.len(), 1);
        let changes = result
            .edit
            .and_then(|edit| edit.changes)
            .expect("replacement should produce changes");
        assert_eq!(changes.len(), 1);
        assert_eq!(
            changes[&path_to_uri(&path).expect("temporary path must convert to URI")][0].new_text,
            "fn main() { bar(2); }\n"
        );
    }

    #[test]
    fn structural_replacement_rejects_invalid_planned_source() {
        let temp = tempfile::tempdir().expect("temporary directory should be created");
        let path = temp.path().join("source.rs");
        fs::write(&path, "fn target() { let value = 1; }\n")
            .expect("fixture source should be written");

        let result = structural_search_sync(
            temp.path(),
            "rust",
            "fn target() { $$$BODY }",
            Some("fn target() { $$$ }"),
            PositionEncoding::Utf8,
            &HashMap::new(),
            false,
            &AtomicBool::new(false),
        );

        assert!(
            result
                .expect_err("invalid replacement output must fail closed")
                .contains("invalid syntax")
        );
    }

    #[test]
    fn structural_parse_only_validates_replacement_variables() {
        let result = structural_search_sync(
            Path::new("."),
            "rust",
            "foo($A)",
            Some("bar($B)"),
            PositionEncoding::Utf8,
            &HashMap::new(),
            true,
            &AtomicBool::new(false),
        );

        assert!(
            result
                .expect_err("undefined replacement variable should fail")
                .contains("undefined metavariables: B")
        );
    }

    #[test]
    fn structural_parse_only_rejects_pattern_parse_errors() {
        let result = structural_search_sync(
            Path::new("."),
            "rust",
            "fn {",
            None,
            PositionEncoding::Utf8,
            &HashMap::new(),
            true,
            &AtomicBool::new(false),
        );

        assert!(result.is_err());
    }

    #[test]
    fn cancelled_structural_search_stops_before_traversal() {
        let result = structural_search_sync(
            Path::new("."),
            "rust",
            "foo($A)",
            None,
            PositionEncoding::Utf8,
            &HashMap::new(),
            false,
            &AtomicBool::new(true),
        );

        assert_eq!(
            result.expect_err("cancelled search should fail"),
            "ast-grep search cancelled"
        );
    }

    #[test]
    fn structural_search_fails_closed_on_overlapping_matches() {
        let Ok(temp) = tempfile::tempdir() else {
            panic!("failed to create temporary directory");
        };
        assert!(fs::write(temp.path().join("overlap.rs"), "fn main() { foo(1); }\n").is_ok());

        let result = structural_search_sync(
            temp.path(),
            "rust",
            "$A",
            None,
            PositionEncoding::Utf8,
            &HashMap::new(),
            false,
            &AtomicBool::new(false),
        );

        assert!(
            result
                .expect_err("overlapping matches should fail")
                .contains("overlapping")
        );
    }

    #[test]
    fn structural_search_skips_generated_directories() {
        let Ok(temp) = tempfile::tempdir() else {
            panic!("failed to create temporary directory");
        };
        let target = temp.path().join("target");
        assert!(fs::create_dir(&target).is_ok());
        assert!(fs::write(target.join("generated.rs"), "fn main() { foo(1); }\n").is_ok());

        let result = structural_search_sync(
            temp.path(),
            "rust",
            "foo($A)",
            None,
            PositionEncoding::Utf8,
            &HashMap::new(),
            false,
            &AtomicBool::new(false),
        )
        .expect("structural search should succeed");

        assert!(result.matches.is_empty());
    }

    #[test]
    fn structural_replacement_supports_non_rust_languages() {
        let Ok(temp) = tempfile::tempdir() else {
            panic!("failed to create temporary directory");
        };
        let path = temp.path().join("source.ts");
        assert!(fs::write(&path, "const value = foo(1);\n").is_ok());
        let path = fs::canonicalize(path).expect("test path should canonicalize");

        let result = structural_search_sync(
            temp.path(),
            "typescript",
            "foo($A)",
            Some("bar($A)"),
            PositionEncoding::Utf8,
            &HashMap::new(),
            false,
            &AtomicBool::new(false),
        )
        .expect("TypeScript replacement should succeed");

        assert_eq!(result.matches.len(), 1);
        let planned = result
            .edit
            .and_then(|edit| edit.changes)
            .and_then(|changes| {
                changes
                    .get(&path_to_uri(&path).expect("temporary path must convert to URI"))
                    .cloned()
            })
            .and_then(|edits| edits.into_iter().next())
            .map(|edit| edit.new_text);
        assert_eq!(planned.as_deref(), Some("const value = bar(1);\n"));
    }

    #[test]
    fn structural_parse_only_enforces_query_byte_limit() {
        let query = "x".repeat(MAX_QUERY_BYTES + 1);
        let result = structural_search_sync(
            Path::new("."),
            "rust",
            &query,
            None,
            PositionEncoding::Utf8,
            &HashMap::new(),
            true,
            &AtomicBool::new(false),
        );

        assert_eq!(
            result.expect_err("oversized query should fail"),
            "ast-grep query exceeded byte limit"
        );
    }

    #[test]
    fn no_match_scan_stops_at_file_budget() {
        let Ok(first_root) = tempfile::tempdir() else {
            panic!("failed to create first temporary directory");
        };
        let Ok(second_root) = tempfile::tempdir() else {
            panic!("failed to create second temporary directory");
        };
        for index in 0..MAX_SCANNED_FILES {
            assert!(
                fs::write(
                    first_root.path().join(format!("{index:04}.rs")),
                    "fn no_match() {}\n"
                )
                .is_ok()
            );
        }
        assert!(
            fs::write(
                second_root.path().join("match.rs"),
                "fn budget_match() {}\n"
            )
            .is_ok()
        );

        let symbols = search_sync(
            &[
                first_root.path().to_path_buf(),
                second_root.path().to_path_buf(),
            ],
            &["rust".to_string()],
            "budget_match",
            None,
            10,
            &AtomicBool::new(false),
        );

        assert!(symbols.is_empty());
    }
}
