//! Bounded lexical matching primitives for project-scoped text search.

use std::{collections::BTreeSet, ops::Range, path::PathBuf};

use globset::{Glob, GlobSet, GlobSetBuilder};
use ignore::WalkBuilder;
use regex::RegexBuilder;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::ast_grep::is_generated_path;
use super::translator::SourceContext;

/// Matching interpretation for a lexical query.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LexicalMatchMode {
    /// Treat the query as literal text.
    Literal,
    /// Interpret the query with Rust regex syntax.
    Regex,
}

/// Case behavior for a lexical query.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LexicalCaseMode {
    /// Match exact case.
    Sensitive,
    /// Ignore case.
    Insensitive,
    /// Ignore case unless the query contains an uppercase character.
    Smart,
}

/// Actor-owned lexical search limits and matching semantics.
#[derive(Debug, Clone)]
pub(crate) struct LexicalSearchRequest {
    /// Text or regex pattern to find.
    pub query: String,
    /// Whether `query` is literal text or a Rust regex.
    pub mode: LexicalMatchMode,
    /// Case behavior for the query.
    pub case: LexicalCaseMode,
    /// Enable multiline regex anchors.
    pub multiline: bool,
    /// Maximum project files to inspect.
    pub max_files: usize,
    /// Maximum matches returned across all files.
    pub max_matches: usize,
    /// Whether generated paths are in scope.
    pub include_generated: bool,
    /// Project-relative globs that include files; empty includes every file.
    pub include_paths: Vec<String>,
    /// Project-relative globs that exclude files after inclusion.
    pub exclude_paths: Vec<String>,
    /// Context lines around each match.
    pub context_lines: usize,
    /// Opaque continuation token returned by an earlier page.
    pub page_token: Option<String>,
}

/// Common options for a caller-ordered lexical query batch.
#[derive(Debug, Clone)]
pub(crate) struct LexicalSearchBatchRequest {
    pub queries: Vec<String>,
    pub mode: LexicalMatchMode,
    pub case: LexicalCaseMode,
    pub multiline: bool,
    pub max_files: usize,
    pub max_matches: usize,
    pub include_generated: bool,
    pub include_paths: Vec<String>,
    pub exclude_paths: Vec<String>,
    pub context_lines: usize,
    pub max_bytes: usize,
}

/// One snapshot-bound lexical match, retaining byte offsets until the MCP
/// boundary chooses its negotiated position encoding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub(crate) struct LexicalSearchMatch {
    /// Canonical project-relative path.
    pub project_relative_path: String,
    /// Open-document version when this came from an unsaved document.
    pub document_version: Option<i32>,
    /// Content hash of the snapshot searched.
    pub content_hash: String,
    /// Snapshot-bound source resource for the exact match range.
    pub source_uri: String,
    /// Optional bounded inline context around the match.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<SourceContext>,
    /// UTF-8 byte range within the returned snapshot.
    pub byte_range: Range<usize>,
}

/// Results of a bounded scan, including accounting for matches that were not
/// retained for the requested page.
#[derive(Debug, Clone)]
pub(crate) struct LexicalSearchScan {
    /// Matches retained for the caller's requested window.
    pub matches: Vec<LexicalSearchMatch>,
    /// Total matches found in the scanned snapshot.
    pub total_matches: usize,
    /// Number of files whose snapshots were read.
    pub scanned_files: usize,
    /// Total UTF-8 bytes examined across those snapshots.
    pub scanned_bytes: usize,
    /// Offset of `matches` within the retained snapshot.
    pub offset: usize,
    /// Opaque token identifying the retained snapshot.
    pub page_token: String,
    /// Hash of the immutable snapshot used for every page.
    pub snapshot_identity: String,
}

/// One caller-ordered entry in a shared-budget lexical batch.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub(crate) struct LexicalSearchBatchEntry {
    pub query: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<LexicalSearchResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reused_from: Option<usize>,
    pub skipped_by_budget: bool,
}

/// Results for several lexical queries scanned from one source snapshot pass.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub(crate) struct LexicalSearchBatchResult {
    pub entries: Vec<LexicalSearchBatchEntry>,
    pub unique_queries: usize,
    pub scanned_files: usize,
    pub scanned_bytes: usize,
    pub returned: usize,
    pub truncated: bool,
    pub max_matches: usize,
    pub max_bytes: usize,
    pub snapshot_identity: String,
}

/// One bounded lexical-search page with deterministic continuation metadata.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub(crate) struct LexicalSearchResult {
    /// Compact identities returned on this page.
    pub matches: Vec<LexicalSearchMatch>,
    /// Number of identities in `matches`.
    pub returned: usize,
    /// Total matches found in the scanned snapshot.
    pub total: usize,
    /// Matches remaining after this page.
    pub remaining: usize,
    /// Number of files scanned to produce this page.
    pub scanned_files: usize,
    /// UTF-8 bytes scanned to produce this page.
    pub scanned_bytes: usize,
    /// Hash of the immutable snapshot used for every page.
    pub snapshot_identity: String,
    /// Effective serialized byte ceiling applied to this page.
    pub max_bytes: usize,
    /// Whether another page is available.
    pub truncated: bool,
    /// Opaque snapshot-owned cursor for the next page, when `truncated`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

/// Find every non-overlapping match using the selected lexical semantics.
///
/// Regex mode uses the Rust `regex` dialect; multiline enables line anchors
/// without enabling dot-all behavior.
pub(crate) fn find_matches(
    text: &str,
    query: &str,
    mode: LexicalMatchMode,
    case: LexicalCaseMode,
    multiline: bool,
) -> Result<Vec<Range<usize>>, String> {
    if query.is_empty() {
        return Err("lexical query must not be empty".to_owned());
    }
    let pattern = match mode {
        LexicalMatchMode::Literal => regex::escape(query),
        LexicalMatchMode::Regex => query.to_owned(),
    };
    let case_insensitive = match case {
        LexicalCaseMode::Sensitive => false,
        LexicalCaseMode::Insensitive => true,
        LexicalCaseMode::Smart => !query.chars().any(char::is_uppercase),
    };
    let regex = RegexBuilder::new(&pattern)
        .case_insensitive(case_insensitive)
        .multi_line(multiline)
        .build()
        .map_err(|error| format!("invalid lexical regex: {error}"))?;
    Ok(regex
        .find_iter(text)
        .map(|matched| matched.start()..matched.end())
        .collect())
}

/// Collect canonical project files with optional project-relative glob filters.
pub(crate) async fn collect_project_paths_filtered(
    roots: &[PathBuf],
    include_generated: bool,
    max_files: usize,
    includes: &[String],
    excludes: &[String],
) -> Result<Vec<PathBuf>, String> {
    if max_files == 0 {
        return Ok(Vec::new());
    }
    let includes = build_glob_set(includes)?;
    let excludes = build_glob_set(excludes)?;
    let has_includes = !includes.is_empty();
    let roots = roots.to_vec();
    Ok(tokio::task::spawn_blocking(move || {
        let mut paths = BTreeSet::new();
        for root in roots {
            for entry in WalkBuilder::new(&root)
                .standard_filters(true)
                .filter_entry(move |entry| include_generated || !is_generated_path(entry.path()))
                .build()
                .flatten()
            {
                if paths.len() >= max_files {
                    break;
                }
                if !entry
                    .file_type()
                    .is_some_and(|file_type| file_type.is_file())
                {
                    continue;
                }
                let Ok(relative) = entry.path().strip_prefix(&root) else {
                    continue;
                };
                if (has_includes && !includes.is_match(relative)) || excludes.is_match(relative) {
                    continue;
                }
                if let Ok(path) = std::fs::canonicalize(entry.path()) {
                    paths.insert(path);
                }
            }
            if paths.len() >= max_files {
                break;
            }
        }
        paths.into_iter().collect()
    })
    .await
    .unwrap_or_default())
}

fn build_glob_set(patterns: &[String]) -> Result<GlobSet, String> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        builder.add(
            Glob::new(pattern)
                .map_err(|error| format!("invalid lexical path glob {pattern:?}: {error}"))?,
        );
    }
    builder
        .build()
        .map_err(|error| format!("invalid lexical path globs: {error}"))
}

/// Reject malformed path filters before queuing an actor request.
pub(crate) fn validate_path_globs(includes: &[String], excludes: &[String]) -> Result<(), String> {
    build_glob_set(includes)?;
    build_glob_set(excludes)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn literal_metacharacters_are_not_regex() {
        let matches = find_matches(
            "a.b\naXb",
            "a.b",
            LexicalMatchMode::Literal,
            LexicalCaseMode::Sensitive,
            false,
        )
        .unwrap();

        assert_eq!(matches, vec![0..3]);
    }

    #[test]
    fn regex_and_smart_case_follow_the_selected_mode() {
        let regex_matches = find_matches(
            "item-12 item-abc",
            r"item-\d+",
            LexicalMatchMode::Regex,
            LexicalCaseMode::Sensitive,
            false,
        )
        .unwrap();
        assert_eq!(regex_matches, vec![0..7]);

        let insensitive = find_matches(
            "Needle needle",
            "needle",
            LexicalMatchMode::Literal,
            LexicalCaseMode::Smart,
            false,
        )
        .unwrap();
        assert_eq!(insensitive, vec![0..6, 7..13]);

        let sensitive = find_matches(
            "Needle needle",
            "Needle",
            LexicalMatchMode::Literal,
            LexicalCaseMode::Smart,
            false,
        )
        .unwrap();
        assert_eq!(sensitive, vec![0..6]);

        let explicit = find_matches(
            "Needle needle",
            "Needle",
            LexicalMatchMode::Literal,
            LexicalCaseMode::Insensitive,
            false,
        )
        .unwrap();
        assert_eq!(explicit, vec![0..6, 7..13]);
    }

    #[tokio::test]
    async fn project_paths_are_ignore_aware_ordered_and_bounded() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join(".gitignore"), "ignored.rs\n").unwrap();
        fs::write(root.path().join("z.rs"), "z").unwrap();
        fs::write(root.path().join("a.rs"), "a").unwrap();
        fs::write(root.path().join("ignored.rs"), "ignored").unwrap();
        fs::create_dir(root.path().join("target")).unwrap();
        fs::write(root.path().join("target/generated.rs"), "generated").unwrap();

        let paths =
            collect_project_paths_filtered(&[root.path().to_path_buf()], false, 2, &[], &[])
                .await
                .unwrap();

        assert_eq!(
            paths,
            vec![
                root.path().join("a.rs").canonicalize().unwrap(),
                root.path().join("z.rs").canonicalize().unwrap(),
            ]
        );
    }

    #[tokio::test]
    async fn project_paths_apply_include_and_exclude_globs() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("src")).unwrap();
        fs::write(root.path().join("src/lib.rs"), "lib").unwrap();
        fs::write(root.path().join("src/test.rs"), "test").unwrap();
        fs::write(root.path().join("README.md"), "readme").unwrap();

        let paths = collect_project_paths_filtered(
            &[root.path().to_path_buf()],
            false,
            16,
            &["src/**/*.rs".to_owned()],
            &["**/test.rs".to_owned()],
        )
        .await
        .unwrap();

        assert_eq!(
            paths,
            vec![root.path().join("src/lib.rs").canonicalize().unwrap()]
        );
    }
}
