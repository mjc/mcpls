use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use super::dto::{
    DeferredResourceReference, Location, Range, SourceContext, SourceFrame, SourceUnavailableReason,
};
use super::encoding_ctx::EncodingCtx;
use crate::bridge::DocumentTracker;
use crate::bridge::resources::{SourceResource, make_source_uri};
use crate::bridge::state::{path_to_uri, uri_to_path};
use crate::error::{Error, Result};

const MAX_FRAME_LINES: usize = 12;
const MAX_FRAME_BYTES: usize = 4 * 1024;
const MAX_RESPONSE_BYTES: usize = 32 * 1024;

#[derive(Debug)]
pub(super) struct SourceBudget {
    remaining_bytes: usize,
    truncated: bool,
}

impl Default for SourceBudget {
    fn default() -> Self {
        Self {
            remaining_bytes: MAX_RESPONSE_BYTES,
            truncated: false,
        }
    }
}

impl SourceBudget {
    pub(super) const fn new(max_bytes: usize) -> Self {
        Self {
            remaining_bytes: max_bytes,
            truncated: false,
        }
    }

    pub(super) const fn truncated(&self) -> bool {
        self.truncated
    }
}

pub(super) async fn resolve_source_context(
    tracker: &DocumentTracker,
    workspace_roots: &[PathBuf],
    approved_source_roots: &[PathBuf],
    uri: &lsp_types::Uri,
    range: Range,
    budget: &mut SourceBudget,
) -> SourceContext {
    resolve_source_context_with_max_lines(
        tracker,
        workspace_roots,
        approved_source_roots,
        uri,
        range,
        budget,
        MAX_FRAME_LINES,
    )
    .await
}

#[allow(clippy::too_many_lines)]
pub(super) async fn resolve_source_context_with_max_lines(
    tracker: &DocumentTracker,
    workspace_roots: &[PathBuf],
    approved_source_roots: &[PathBuf],
    uri: &lsp_types::Uri,
    range: Range,
    budget: &mut SourceBudget,
    max_lines: usize,
) -> SourceContext {
    let Some(path) = uri_to_path(uri) else {
        return unavailable(SourceUnavailableReason::NonFileUri);
    };
    let document = tracker.get(&path);
    let canonical_path = if document.is_some() {
        snapshot_authorized_path(&path, workspace_roots, approved_source_roots)
    } else {
        canonical_authorized_path(&path, workspace_roots, approved_source_roots)
    };
    let Some(canonical_path) = canonical_path else {
        return unavailable(if path.exists() || document.is_some() {
            SourceUnavailableReason::OutsideApprovedRoots
        } else {
            SourceUnavailableReason::NotFound
        });
    };
    let (content, language_id, document_version) = if let Some(document) = document {
        (
            document.content().to_owned(),
            Some(document.language_id().to_owned()),
            Some(document.version()),
        )
    } else {
        let Ok(content) = tokio::fs::read_to_string(&canonical_path).await else {
            return unavailable(if canonical_path.exists() {
                SourceUnavailableReason::Unreadable
            } else {
                SourceUnavailableReason::NotFound
            });
        };
        (content, language_id(&canonical_path), None)
    };

    let content_hash = format!("{:x}", Sha256::digest(content.as_bytes()));
    let deferred = || {
        make_source_uri(
            &canonical_path,
            range.start.line,
            range.start.character,
            range.end.line,
            range.end.character,
            &content_hash,
            document_version,
        )
        .ok()
        .map(|uri| DeferredResourceReference {
            uri,
            kind: "source_context".to_owned(),
            snapshot_hash: content_hash.clone(),
            document_version,
            total_bytes: Some(content.len()),
        })
    };
    if budget.remaining_bytes == 0 {
        budget.truncated = true;
        return deferred().map_or_else(
            || unavailable(SourceUnavailableReason::ResponseBudgetExhausted),
            |resource| SourceContext::Deferred { resource },
        );
    }

    let lines: Vec<&str> = content.lines().collect();
    let total_lines = lines.len();
    let target = range.start.line.saturating_sub(1) as usize;
    let start = declaration_context_start(&lines, target.min(total_lines));
    let end = start.saturating_add(max_lines).min(total_lines);
    let selected = &lines[start.min(total_lines)..end];
    let total_bytes = lines
        .iter()
        .enumerate()
        .map(|(offset, line)| numbered_line_bytes(offset + 1, line))
        .sum();
    let byte_limit = MAX_FRAME_BYTES.min(budget.remaining_bytes);
    let mut text = String::new();
    let mut returned_lines = 0;
    for (offset, line) in selected.iter().enumerate() {
        let rendered = format!("{:>4} | {line}\n", start + offset + 1);
        if text.len() + rendered.len() > byte_limit {
            break;
        }
        text.push_str(&rendered);
        returned_lines += 1;
    }
    budget.remaining_bytes = if text.is_empty() && !selected.is_empty() {
        0
    } else {
        budget.remaining_bytes.saturating_sub(text.len())
    };
    let returned_bytes = text.len();
    let canonical_uri =
        path_to_uri(&canonical_path).map_or_else(|_| uri.to_string(), |uri| uri.to_string());

    let truncated = start > 0 || returned_lines < selected.len() || end < total_lines;
    budget.truncated |= truncated;
    let resource = truncated.then(deferred).flatten();
    SourceContext::Available(SourceFrame {
        path: canonical_path.to_string_lossy().into_owned(),
        uri: canonical_uri,
        highlighted_range: range.clone(),
        range,
        text,
        language_id,
        document_version,
        content_hash,
        returned_lines,
        total_lines,
        returned_bytes,
        total_bytes,
        truncated,
        resource,
    })
}

fn declaration_context_start(lines: &[&str], target: usize) -> usize {
    let mut start = target;
    while let Some(previous) = start.checked_sub(1).and_then(|index| lines.get(index)) {
        let previous = previous.trim_start();
        if previous.starts_with("///") || previous.starts_with("//! ") || previous.starts_with("#[")
        {
            start -= 1;
        } else {
            break;
        }
    }
    start
}

const fn unavailable(reason: SourceUnavailableReason) -> SourceContext {
    SourceContext::Unavailable { reason }
}

fn canonical_authorized_path(
    path: &Path,
    roots: &[PathBuf],
    approved: &[PathBuf],
) -> Option<PathBuf> {
    let path = dunce::canonicalize(path).ok()?;
    authorized(&path, roots, approved).then_some(path)
}

fn snapshot_authorized_path(
    path: &Path,
    roots: &[PathBuf],
    approved: &[PathBuf],
) -> Option<PathBuf> {
    if path.exists() {
        return canonical_authorized_path(path, roots, approved);
    }
    let parent = dunce::canonicalize(path.parent()?).ok()?;
    let canonical = parent.join(path.file_name()?);
    authorized(&canonical, roots, approved).then_some(canonical)
}

fn authorized(path: &Path, roots: &[PathBuf], approved: &[PathBuf]) -> bool {
    roots.iter().chain(approved).any(|root| {
        dunce::canonicalize(root)
            .ok()
            .is_some_and(|root| path.starts_with(root))
    })
}

fn numbered_line_bytes(line_number: usize, line: &str) -> usize {
    format!("{line_number:>4} | {line}\n").len()
}

impl EncodingCtx {
    pub(super) async fn source_context(
        &self,
        workspace_roots: &[PathBuf],
        uri: &lsp_types::Uri,
        range: Range,
        budget: &mut SourceBudget,
    ) -> SourceContext {
        resolve_source_context(&self.tracker, workspace_roots, &[], uri, range, budget).await
    }

    pub(super) async fn source_context_with_max_lines(
        &self,
        workspace_roots: &[PathBuf],
        uri: &lsp_types::Uri,
        range: Range,
        budget: &mut SourceBudget,
        max_lines: usize,
    ) -> SourceContext {
        resolve_source_context_with_max_lines(
            &self.tracker,
            workspace_roots,
            &[],
            uri,
            range,
            budget,
            max_lines,
        )
        .await
    }

    pub(super) async fn location(
        &self,
        workspace_roots: &[PathBuf],
        location: lsp_types::Location,
        budget: &mut SourceBudget,
    ) -> Location {
        let range = self.normalize_range(&location.uri, location.range).await;
        let source = self
            .source_context(workspace_roots, &location.uri, range.clone(), budget)
            .await;
        Location {
            path: uri_to_path(&location.uri).map(|path| path.to_string_lossy().into_owned()),
            uri: location.uri.to_string(),
            range,
            source,
            symbol_handle: None,
        }
    }
}

fn source_snapshot_matches(
    resource: &SourceResource,
    current_version: Option<i32>,
    current_hash: &str,
) -> bool {
    resource.snapshot_hash == current_hash
        && resource
            .document_version
            .is_none_or(|version| Some(version) == current_version)
}

impl super::Translator {
    pub(crate) async fn read_source_resource(
        &self,
        resource: &SourceResource,
    ) -> Result<SourceFrame> {
        let (path, version, hash, _) = self.source_snapshot(&resource.path).await?;
        if !source_snapshot_matches(resource, version, &hash) {
            return Err(Error::McpServer(
                "stale_resource: source snapshot changed; rerun the semantic request".to_owned(),
            ));
        }
        let uri = path_to_uri(&path)?;
        let range = Range {
            start: super::dto::Position2D {
                line: resource.start_line,
                character: resource.start_character,
            },
            end: super::dto::Position2D {
                line: resource.end_line,
                character: resource.end_character,
            },
        };
        let mut budget = SourceBudget::new(usize::MAX);
        let source = resolve_source_context_with_max_lines(
            &self.document_tracker,
            &self.workspace_roots,
            &[],
            &uri,
            range,
            &mut budget,
            usize::MAX,
        )
        .await;
        let SourceContext::Available(mut frame) = source else {
            return Err(Error::McpServer(
                "deferred source resource is no longer readable".to_owned(),
            ));
        };
        frame.resource = None;
        frame.truncated = false;
        Ok(frame)
    }

    pub(crate) async fn source_snapshot(
        &self,
        path: &Path,
    ) -> crate::error::Result<(PathBuf, Option<i32>, String, String)> {
        let path = self.validate_path(path)?;
        if let Some(document) = self.document_tracker.get(&path) {
            let content = document.content().to_owned();
            return Ok((
                path,
                Some(document.version()),
                format!("{:x}", Sha256::digest(content.as_bytes())),
                content,
            ));
        }
        let content = tokio::fs::read_to_string(&path).await?;
        let hash = format!("{:x}", Sha256::digest(content.as_bytes()));
        Ok((path, None, hash, content))
    }
}

fn language_id(path: &Path) -> Option<String> {
    match path.extension()?.to_str()? {
        "rs" => Some("rust".to_owned()),
        "py" => Some("python".to_owned()),
        "js" => Some("javascript".to_owned()),
        "ts" => Some("typescript".to_owned()),
        _ => None,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::bridge::state::ResourceLimits;
    use crate::bridge::translator::{Position2D, SourceUnavailableReason};

    fn range(line: u32) -> Range {
        Range {
            start: Position2D { line, character: 1 },
            end: Position2D { line, character: 2 },
        }
    }

    #[test]
    fn unversioned_source_resource_accepts_matching_open_snapshot() {
        let resource = SourceResource {
            path: "/workspace/src/lib.rs".into(),
            start_line: 1,
            start_character: 1,
            end_line: 1,
            end_character: 2,
            snapshot_hash: "abc".to_owned(),
            document_version: None,
        };

        assert!(source_snapshot_matches(&resource, Some(7), "abc"));
        assert!(!source_snapshot_matches(&resource, Some(7), "changed"));

        let versioned = SourceResource {
            document_version: Some(7),
            ..resource
        };
        assert!(!source_snapshot_matches(&versioned, Some(8), "abc"));
    }

    #[tokio::test]
    async fn resolves_authorized_disk_source_with_complete_metadata() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("main.rs");
        tokio::fs::write(&path, "fn main() {}\r\n// λ\r\n")
            .await
            .unwrap();
        let uri = path_to_uri(&path).unwrap();
        let tracker = DocumentTracker::new(ResourceLimits::default(), HashMap::new());
        let source = resolve_source_context(
            &tracker,
            &[root.path().into()],
            &[],
            &uri,
            range(2),
            &mut SourceBudget::default(),
        )
        .await;
        let SourceContext::Available(frame) = source else {
            panic!("source unavailable")
        };
        assert_eq!(frame.language_id.as_deref(), Some("rust"));
        assert_eq!(frame.document_version, None);
        assert!(frame.text.contains("2 | // λ"));
        assert_eq!(
            frame.path,
            dunce::canonicalize(path).unwrap().to_string_lossy()
        );
        assert!(!frame.content_hash.is_empty());
    }

    #[tokio::test]
    async fn source_frame_includes_declaration_docs_header_and_body() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("lib.rs");
        tokio::fs::write(
            &path,
            "//! module docs\n\n/// First line.\n/// Second line.\n#[must_use]\npub fn answer() -> u32 {\n    42\n}\n",
        )
        .await
        .unwrap();
        let uri = path_to_uri(&path).unwrap();
        let tracker = DocumentTracker::new(ResourceLimits::default(), HashMap::new());

        let source = resolve_source_context(
            &tracker,
            &[root.path().into()],
            &[],
            &uri,
            range(6),
            &mut SourceBudget::default(),
        )
        .await;

        let SourceContext::Available(frame) = source else {
            panic!("source unavailable")
        };
        assert!(frame.text.contains("3 | /// First line."), "{}", frame.text);
        assert!(
            frame.text.contains("4 | /// Second line."),
            "{}",
            frame.text
        );
        assert!(frame.text.contains("5 | #[must_use]"), "{}", frame.text);
        assert!(frame.text.contains("6 | pub fn answer()"), "{}", frame.text);
        assert!(frame.text.contains("7 |     42"), "{}", frame.text);
    }

    #[tokio::test]
    async fn rejects_non_file_and_outside_root_uris() {
        let tracker = DocumentTracker::new(ResourceLimits::default(), HashMap::new());
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        let outside_uri = path_to_uri(outside.path()).unwrap();
        let source = resolve_source_context(
            &tracker,
            &[root.path().into()],
            &[],
            &outside_uri,
            range(1),
            &mut SourceBudget::default(),
        )
        .await;
        assert!(matches!(
            source,
            SourceContext::Unavailable {
                reason: SourceUnavailableReason::OutsideApprovedRoots
            }
        ));
        let uri: lsp_types::Uri = "jar:file:///tmp/a.jar!/A.java".parse().unwrap();
        let source = resolve_source_context(
            &tracker,
            &[root.path().into()],
            &[],
            &uri,
            range(1),
            &mut SourceBudget::default(),
        )
        .await;
        assert!(matches!(
            source,
            SourceContext::Unavailable {
                reason: SourceUnavailableReason::NonFileUri
            }
        ));
    }

    #[tokio::test]
    async fn approved_dependency_root_is_allowed_and_response_budget_is_shared() {
        let workspace = tempfile::tempdir().unwrap();
        let dependency = tempfile::tempdir().unwrap();
        let path = dependency.path().join("lib.rs");
        tokio::fs::write(&path, "x\n").await.unwrap();
        let uri = path_to_uri(&path).unwrap();
        let tracker = DocumentTracker::new(ResourceLimits::default(), HashMap::new());
        let mut budget = SourceBudget {
            remaining_bytes: 8,
            truncated: false,
        };
        let source = resolve_source_context(
            &tracker,
            &[workspace.path().into()],
            &[dependency.path().into()],
            &uri,
            range(1),
            &mut budget,
        )
        .await;
        assert!(matches!(source, SourceContext::Available(_)));
        let source = resolve_source_context(
            &tracker,
            &[workspace.path().into()],
            &[dependency.path().into()],
            &uri,
            range(1),
            &mut budget,
        )
        .await;
        assert!(matches!(source, SourceContext::Deferred { .. }));
    }

    #[tokio::test]
    async fn tracked_dirty_snapshot_wins_over_disk_and_carries_version() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("dirty.rs");
        tokio::fs::write(&path, "disk\n").await.unwrap();
        let tracker = DocumentTracker::new(ResourceLimits::default(), HashMap::new());
        let uri = tracker.open(path.clone(), "initial\n".to_owned()).unwrap();
        assert_eq!(tracker.update(&path, "dirty λ\n".to_owned()), Some(2));
        tokio::fs::remove_file(&path).await.unwrap();

        let source = resolve_source_context(
            &tracker,
            &[root.path().into()],
            &[],
            &uri,
            range(1),
            &mut SourceBudget::default(),
        )
        .await;
        let SourceContext::Available(frame) = source else {
            panic!("source unavailable")
        };
        assert!(frame.text.contains("dirty λ"));
        assert!(!frame.text.contains("disk"));
        assert_eq!(frame.document_version, Some(2));
    }

    #[tokio::test]
    async fn truncation_never_splits_utf8_and_reports_exact_counts() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("wide.rs");
        let content = format!("{}\nsecond\n", "λ".repeat(MAX_FRAME_BYTES));
        tokio::fs::write(&path, &content).await.unwrap();
        let tracker = DocumentTracker::new(ResourceLimits::default(), HashMap::new());
        let uri = path_to_uri(&path).unwrap();
        let source = resolve_source_context(
            &tracker,
            &[root.path().into()],
            &[],
            &uri,
            range(1),
            &mut SourceBudget::default(),
        )
        .await;
        let SourceContext::Available(frame) = source else {
            panic!("source unavailable")
        };
        assert!(frame.truncated);
        assert_eq!(frame.returned_bytes, frame.text.len());
        assert_eq!(frame.returned_lines, frame.text.lines().count());
        assert_eq!(frame.total_lines, 2);
        assert!(frame.total_bytes > frame.returned_bytes);
    }
}
