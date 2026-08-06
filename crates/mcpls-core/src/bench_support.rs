//! Narrow adapters for benchmarks of otherwise private implementation details.

use std::path::{Path, PathBuf};

/// Count native watch directories selected for recursive globs.
///
/// # Errors
///
/// Returns an error when the benchmark fixture or glob is invalid.
pub fn desired_watch_directory_count(root: &Path, patterns: &[&str]) -> Result<usize, String> {
    crate::lsp::watcher::benchmark_desired_watch_directory_count(root, patterns)
        .map_err(|error| format!("{error:?}"))
}

/// Build rust-analyzer initialization options for the supplied roots.
///
/// # Errors
///
/// Returns an error when a root has no Cargo manifest or the options are invalid.
pub fn rust_analyzer_initialization_options(
    roots: &[PathBuf],
) -> Result<serde_json::Value, String> {
    crate::bridge::translator::benchmark_rust_analyzer_initialization_options(roots)
        .map_err(|error| error.to_string())
}

/// Count Rust workspace-symbol fallback results for a query.
#[must_use]
pub fn ast_workspace_symbol_count(root: &Path, query: &str, limit: usize) -> usize {
    crate::bridge::ast_grep::benchmark_workspace_symbol_count(root, query, limit)
}
