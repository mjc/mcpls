//! In-process AST-grep fallback for workspace symbol lookup.

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use ast_grep_core::Node;
use ast_grep_core::tree_sitter::{LanguageExt, StrDoc};
use ast_grep_language::{Language, SupportLang};
use ignore::WalkBuilder;

use super::encoding::{EncodingConverter, PositionEncoding};

const MAX_SCANNED_FILES: usize = 4_096;
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_TOTAL_BYTES: u64 = 64 * 1024 * 1024;
const MAX_SCAN_DURATION: Duration = Duration::from_secs(10);
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
    if query.is_empty() || limit == 0 {
        return Vec::new();
    }

    let roots = roots.to_vec();
    let languages = languages.to_vec();
    let query = query.to_string();
    let kind_filter = kind_filter.map(str::to_owned);
    tokio::task::spawn_blocking(move || {
        search_sync(&roots, &languages, &query, kind_filter.as_deref(), limit)
    })
    .await
    .unwrap_or_default()
}

#[allow(clippy::too_many_lines)]
fn search_sync(
    roots: &[PathBuf],
    languages: &[String],
    query: &str,
    kind_filter: Option<&str>,
    limit: usize,
) -> Vec<Symbol> {
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
            if started.elapsed() >= MAX_SCAN_DURATION || paths.len() >= MAX_SCANNED_FILES {
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
        if started.elapsed() >= MAX_SCAN_DURATION || paths.len() >= MAX_SCANNED_FILES {
            break;
        }
    }

    let mut symbols = Vec::new();
    let mut total_bytes: u64 = 0;
    for path in paths {
        if started.elapsed() >= MAX_SCAN_DURATION {
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

        let tree = language.ast_grep(&source);
        for node in tree.root().dfs() {
            let Some(kind) = symbol_kind(&node) else {
                continue;
            };
            if kind_filter.is_some_and(|filter| !kind.eq_ignore_ascii_case(filter)) {
                continue;
            }
            let Some(name) = symbol_name(&node) else {
                continue;
            };
            if !name.to_ascii_lowercase().contains(&query) {
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

/// Convert tree-sitter byte offsets to the daemon's default UTF-8 MCP units.
/// Fallback results have no negotiated LSP encoding, so using the explicit
/// default keeps Unicode coordinates deterministic rather than relying on the
/// parser's point-column convention.
fn byte_offset_to_fallback_position(source: &str, offset: usize) -> Option<(u32, u32)> {
    if offset > source.len() || !source.is_char_boundary(offset) {
        return None;
    }
    let line = source[..offset]
        .bytes()
        .filter(|byte| *byte == b'\n')
        .count();
    let line_start = source[..offset].rfind('\n').map_or(0, |index| index + 1);
    let character = EncodingConverter::new(PositionEncoding::Utf8)
        .byte_offset_to_character(&source[line_start..offset], offset - line_start)
        .ok()?;
    Some((u32::try_from(line).ok()?, character))
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
mod tests {
    use super::*;

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
        );

        assert_eq!(symbols.len(), 2);
        assert_eq!(symbols[0].name, "AstFallback");
        assert_eq!(symbols[0].kind, "struct");
        assert_eq!(symbols[1].name, "fallback_function");
        assert_eq!(symbols[1].kind, "function");
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
}
