//! Read-only planning for LSP `WorkspaceEdit` values.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use lsp_types::WorkspaceEdit;

use crate::bridge::{DocumentTracker, PositionEncoding, uri_to_path};
use crate::edit_paths::{
    FileOperation, OperationValidationError, PathSafetyError, WorkspaceBoundary,
};
use crate::edit_plan::{EditLimits, EditPlan, FileSnapshot, SnapshotSource};
use crate::edit_planner::apply_text_edits;
use crate::workspace_edit::{EditOperation, NormalizedWorkspaceEdit, normalize};

/// Bounded resource limits enforced while constructing a preview.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PreviewLimits {
    /// Maximum number of affected files.
    pub max_files: usize,
    /// Maximum number of text edits.
    pub max_edits: usize,
    /// Maximum combined original and planned content bytes.
    pub max_bytes: usize,
}

impl Default for PreviewLimits {
    fn default() -> Self {
        Self {
            max_files: EditLimits::PROJECT.max_files,
            max_edits: EditLimits::PROJECT.max_edits,
            max_bytes: EditLimits::PROJECT.max_bytes,
        }
    }
}

/// Preview data returned before a plan is stored in the project actor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewArtifact {
    /// Bounded, opaque plan retained for explicit application.
    pub plan: EditPlan,
    /// Canonical files touched by text edits.
    pub affected_files: Vec<PathBuf>,
    /// Conflicts detected while planning.
    pub conflicts: Vec<String>,
    /// Valid but not-yet-applicable operations.
    pub unsupported: Vec<String>,
}

/// Errors that prevent a workspace edit from being previewed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PreviewError {
    /// A workspace edit URI was not an absolute file URI.
    #[error("workspace edit URI is not an absolute file URI: {0}")]
    InvalidUri(String),
    /// A path failed canonical workspace validation.
    #[error(transparent)]
    Path(#[from] PathSafetyError),
    /// A resource operation failed its workspace precondition.
    #[error(transparent)]
    Operation(#[from] OperationValidationError),
    /// A text document could not be read from disk.
    #[error("failed to read workspace edit target {path}: {source}")]
    Read {
        /// Target path.
        path: PathBuf,
        /// Underlying filesystem error.
        #[source]
        source: std::io::Error,
    },
    /// A preview exceeded one configured bound.
    #[error("workspace edit preview exceeds {kind} limit: {actual} > {limit}")]
    Limit {
        /// Name of the exceeded limit.
        kind: &'static str,
        /// Observed amount.
        actual: usize,
        /// Configured maximum.
        limit: usize,
    },
}

struct PlannedFile {
    source: SnapshotSource,
    version: Option<i32>,
    original: String,
    planned: String,
}

/// Build a write-free plan from one LSP workspace edit.
///
/// Text edits are applied against the exact disk or open-document contents
/// observed during preview. Resource operations are path-validated and
/// reported as unsupported until the corresponding transactional applier is
/// enabled, so they can never silently turn into arbitrary file writes.
///
/// # Errors
///
/// Returns an error when an edit URI is invalid, a target cannot be read or
/// contained, or a configured preview bound is exceeded.
///
/// # Panics
///
/// Panics only if the internal normalization enum gains a variant without a
/// corresponding preview representation.
pub fn preview_workspace_edit(
    boundary: &WorkspaceBoundary,
    project_id: &str,
    edit: WorkspaceEdit,
    encoding: PositionEncoding,
    documents: &DocumentTracker,
    limits: PreviewLimits,
) -> Result<PreviewArtifact, PreviewError> {
    let normalized = normalize(edit).expect("workspace edit normalization is infallible");
    preview_normalized(
        boundary, project_id, normalized, encoding, documents, limits,
    )
}

fn preview_normalized(
    boundary: &WorkspaceBoundary,
    project_id: &str,
    normalized: NormalizedWorkspaceEdit,
    encoding: PositionEncoding,
    documents: &DocumentTracker,
    limits: PreviewLimits,
) -> Result<PreviewArtifact, PreviewError> {
    PreviewBuilder::new(boundary, project_id, encoding, documents, limits).finish(normalized)
}

struct PreviewBuilder<'a> {
    boundary: &'a WorkspaceBoundary,
    project_id: &'a str,
    encoding: PositionEncoding,
    documents: &'a DocumentTracker,
    limits: PreviewLimits,
    files: BTreeMap<PathBuf, PlannedFile>,
    operations: Vec<String>,
    conflicts: Vec<String>,
    unsupported: Vec<String>,
    edit_count: usize,
}

impl<'a> PreviewBuilder<'a> {
    const fn new(
        boundary: &'a WorkspaceBoundary,
        project_id: &'a str,
        encoding: PositionEncoding,
        documents: &'a DocumentTracker,
        limits: PreviewLimits,
    ) -> Self {
        Self {
            boundary,
            project_id,
            encoding,
            documents,
            limits,
            files: BTreeMap::new(),
            operations: Vec::new(),
            conflicts: Vec::new(),
            unsupported: Vec::new(),
            edit_count: 0,
        }
    }

    fn finish(
        mut self,
        normalized: NormalizedWorkspaceEdit,
    ) -> Result<PreviewArtifact, PreviewError> {
        for operation in normalized.operations {
            self.handle_operation(operation)?;
        }
        if self.files.len() > self.limits.max_files {
            return Err(PreviewError::Limit {
                kind: "file",
                actual: self.files.len(),
                limit: self.limits.max_files,
            });
        }
        let total_bytes = self
            .files
            .values()
            .map(|file| file.original.len().saturating_add(file.planned.len()))
            .sum::<usize>();
        if total_bytes > self.limits.max_bytes {
            return Err(PreviewError::Limit {
                kind: "byte",
                actual: total_bytes,
                limit: self.limits.max_bytes,
            });
        }
        if self.operations.is_empty() {
            self.conflicts
                .push("workspace edit contains no operations".to_string());
        }

        let snapshots = self
            .files
            .into_iter()
            .map(|(path, file)| {
                FileSnapshot::from_contents(
                    path,
                    file.source,
                    file.version,
                    file.original,
                    file.planned,
                )
            })
            .collect::<Vec<_>>();
        let safe_to_apply = self.conflicts.is_empty() && self.unsupported.is_empty();
        let affected_files = snapshots.iter().map(|file| file.path().clone()).collect();
        let plan = EditPlan::new(
            self.project_id.to_string(),
            snapshots,
            self.operations,
            safe_to_apply,
            EditLimits::PROJECT.plan_ttl,
        );
        Ok(PreviewArtifact {
            plan,
            affected_files,
            conflicts: self.conflicts,
            unsupported: self.unsupported,
        })
    }

    fn handle_operation(&mut self, operation: EditOperation) -> Result<(), PreviewError> {
        match operation {
            EditOperation::Text {
                uri,
                version,
                edits,
            } => self.handle_text(&uri.to_string(), version, &edits),
            EditOperation::Create { uri, options, .. } => {
                let path = self
                    .boundary
                    .validate_target(path_for_uri(&uri.to_string())?)?;
                let overwrite = options
                    .as_ref()
                    .and_then(|options| options.overwrite)
                    .unwrap_or(false);
                self.boundary.validate_operations(&[FileOperation::Create {
                    path: path.clone(),
                    overwrite,
                }])?;
                let operation = format!("create {}", path.display());
                self.record_unsupported(&operation);
                Ok(())
            }
            EditOperation::Rename {
                old_uri,
                new_uri,
                options,
                ..
            } => {
                let from = self
                    .boundary
                    .validate_existing(path_for_uri(&old_uri.to_string())?)?;
                let to = self
                    .boundary
                    .validate_target(path_for_uri(&new_uri.to_string())?)?;
                let overwrite = options
                    .as_ref()
                    .and_then(|options| options.overwrite)
                    .unwrap_or(false);
                self.boundary.validate_operations(&[FileOperation::Rename {
                    from: from.clone(),
                    to: to.clone(),
                    overwrite,
                }])?;
                let operation = format!("rename {} -> {}", from.display(), to.display());
                self.record_unsupported(&operation);
                Ok(())
            }
            EditOperation::Delete { uri, options, .. } => {
                let path = self
                    .boundary
                    .validate_existing(path_for_uri(&uri.to_string())?)?;
                let recursive = options
                    .as_ref()
                    .and_then(|options| options.recursive)
                    .unwrap_or(false);
                self.boundary.validate_operations(&[FileOperation::Delete {
                    path: path.clone(),
                    recursive,
                }])?;
                let operation = format!("delete {}", path.display());
                self.record_unsupported(&operation);
                Ok(())
            }
        }
    }

    fn handle_text(
        &mut self,
        uri: &str,
        version: Option<i32>,
        edits: &[crate::workspace_edit::NormalizedTextEdit],
    ) -> Result<(), PreviewError> {
        self.edit_count = self.edit_count.saturating_add(edits.len());
        if self.edit_count > self.limits.max_edits {
            return Err(PreviewError::Limit {
                kind: "edit",
                actual: self.edit_count,
                limit: self.limits.max_edits,
            });
        }
        let path = self.boundary.validate_existing(path_for_uri(uri)?)?;
        if !self.files.contains_key(&path) {
            self.files
                .insert(path.clone(), initial_file(&path, self.documents)?);
        }
        let entry = self
            .files
            .get_mut(&path)
            .ok_or_else(|| PreviewError::Limit {
                kind: "file",
                actual: self.limits.max_files.saturating_add(1),
                limit: self.limits.max_files,
            })?;
        let mut conflicts = Vec::new();
        if let Some(expected) = version {
            if entry.source != SnapshotSource::OpenDocument {
                conflicts.push(format!(
                    "versioned edit requires an open document: {}",
                    path.display()
                ));
            } else if entry.version != Some(expected) {
                conflicts.push(format!(
                    "document version changed for {}: expected {}, got {:?}",
                    path.display(),
                    expected,
                    entry.version
                ));
            }
        }
        match apply_text_edits(&entry.planned, edits, self.encoding) {
            Ok(planned) => entry.planned = planned,
            Err(error) => conflicts.push(format!("{}: {error}", path.display())),
        }
        self.conflicts.extend(conflicts);
        self.operations.push(format!("text {}", path.display()));
        Ok(())
    }

    fn record_unsupported(&mut self, operation: &str) {
        self.operations.push(operation.to_string());
        self.unsupported.push(format!(
            "{operation}: resource operation application is not enabled"
        ));
    }
}

fn path_for_uri(uri: &str) -> Result<PathBuf, PreviewError> {
    let parsed = uri
        .parse::<lsp_types::Uri>()
        .map_err(|_| PreviewError::InvalidUri(uri.to_string()))?;
    uri_to_path(&parsed).ok_or_else(|| PreviewError::InvalidUri(uri.to_string()))
}

fn initial_file(path: &PathBuf, documents: &DocumentTracker) -> Result<PlannedFile, PreviewError> {
    if let Some(document) = documents.get(path) {
        return Ok(PlannedFile {
            source: SnapshotSource::OpenDocument,
            version: Some(document.version),
            original: document.content.clone(),
            planned: document.content.clone(),
        });
    }
    let original = fs::read_to_string(path).map_err(|source| PreviewError::Read {
        path: path.clone(),
        source,
    })?;
    Ok(PlannedFile {
        source: SnapshotSource::Disk,
        version: None,
        planned: original.clone(),
        original,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::collections::HashMap;

    use tempfile::TempDir;

    use super::*;
    use crate::bridge::path_to_uri;

    #[test]
    fn previews_disk_text_edit_without_writing() {
        let root = TempDir::new().unwrap();
        let file = root.path().join("src.rs");
        fs::write(&file, "before\n").unwrap();
        let boundary = WorkspaceBoundary::new(root.path()).unwrap();
        let edit = WorkspaceEdit {
            changes: Some(HashMap::from([(
                path_to_uri(&file),
                vec![lsp_types::TextEdit {
                    range: lsp_types::Range::new(
                        lsp_types::Position::new(0, 0),
                        lsp_types::Position::new(0, 6),
                    ),
                    new_text: "after".to_string(),
                }],
            )])),
            document_changes: None,
            change_annotations: None,
        };

        let artifact = preview_workspace_edit(
            &boundary,
            "project",
            edit,
            PositionEncoding::Utf8,
            &DocumentTracker::new(crate::bridge::ResourceLimits::default(), HashMap::new()),
            PreviewLimits::default(),
        )
        .unwrap();
        assert!(artifact.plan.safe_to_apply());
        assert!(artifact.plan.unified_diff().contains("+after"));
        assert_eq!(fs::read_to_string(file).unwrap(), "before\n");
    }
}
