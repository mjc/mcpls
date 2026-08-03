//! In-process AST-grep fallback for workspace symbol lookup.

use std::fs;
use std::path::PathBuf;

use ast_grep_core::Node;
use ast_grep_core::tree_sitter::{LanguageExt, StrDoc};
use ast_grep_language::{Language, SupportLang};
use ignore::WalkBuilder;

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
    limit: usize,
) -> Vec<Symbol> {
    if query.is_empty() || limit == 0 {
        return Vec::new();
    }

    let roots = roots.to_vec();
    let languages = languages.to_vec();
    let query = query.to_string();
    tokio::task::spawn_blocking(move || search_sync(&roots, &languages, &query, limit))
        .await
        .unwrap_or_default()
}

fn search_sync(roots: &[PathBuf], languages: &[String], query: &str, limit: usize) -> Vec<Symbol> {
    let languages = languages
        .iter()
        .filter_map(|language| ast_grep_language(language))
        .collect::<Vec<_>>();
    if languages.is_empty() {
        return Vec::new();
    }

    let query = query.to_ascii_lowercase();
    let mut symbols = Vec::new();
    for root in roots {
        for entry in WalkBuilder::new(root)
            .standard_filters(true)
            .build()
            .flatten()
        {
            let path = entry.path();
            let Some(language) = languages
                .iter()
                .find(|language| SupportLang::from_path(path) == Some(**language))
                .copied()
            else {
                continue;
            };
            let Ok(source) = fs::read_to_string(path) else {
                continue;
            };

            let tree = language.ast_grep(&source);
            for node in tree.root().dfs() {
                let Some(kind) = symbol_kind(&node) else {
                    continue;
                };
                let Some(name) = symbol_name(&node) else {
                    continue;
                };
                if !name.to_ascii_lowercase().contains(&query) {
                    continue;
                }
                let start = node.start_pos();
                let end = node.end_pos();
                let (Some(start_line), Some(start_character), Some(end_line), Some(end_character)) = (
                    u32::try_from(start.line()).ok(),
                    u32::try_from(start.column(&node)).ok(),
                    u32::try_from(end.line()).ok(),
                    u32::try_from(end.column(&node)).ok(),
                ) else {
                    continue;
                };
                symbols.push(Symbol {
                    name,
                    kind: kind.to_string(),
                    path: path.to_path_buf(),
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
    }
    symbols
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
    if kind.contains("method") {
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
            10,
        );

        assert_eq!(symbols.len(), 2);
        assert_eq!(symbols[0].name, "AstFallback");
        assert_eq!(symbols[0].kind, "struct");
        assert_eq!(symbols[1].name, "fallback_function");
        assert_eq!(symbols[1].kind, "function");
    }
}
