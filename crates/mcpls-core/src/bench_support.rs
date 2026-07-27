//! Narrow adapters for benchmarks of otherwise private implementation details.

use std::path::{Path, PathBuf};

/// Count native watch directories selected for one recursive glob.
///
/// # Errors
///
/// Returns an error when the benchmark fixture or glob is invalid.
pub fn desired_watch_directory_count(root: &Path, pattern: &str) -> Result<usize, String> {
    crate::lsp::watcher::benchmark_desired_watch_directory_count(root, pattern)
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
