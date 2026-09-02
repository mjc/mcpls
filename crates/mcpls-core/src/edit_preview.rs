//! Read-only planning for LSP `WorkspaceEdit` values.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::PathBuf;

use lsp_types::WorkspaceEdit;

use crate::bridge::{DocumentSnapshot, DocumentTracker, PositionEncoding, uri_to_path};
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
    /// Maximum combined original and planned bytes for one file.
    pub max_file_bytes: usize,
    /// Maximum create, rename, and delete operations.
    pub max_resource_operations: usize,
}

impl Default for PreviewLimits {
    fn default() -> Self {
        Self {
            max_files: EditLimits::PROJECT.max_files,
            max_edits: EditLimits::PROJECT.max_edits,
            max_bytes: EditLimits::PROJECT.max_bytes,
            max_file_bytes: EditLimits::PROJECT.max_file_bytes,
            max_resource_operations: EditLimits::PROJECT.max_resource_operations,
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
    /// Optional semantic verification outcome for a specialized refactor.
    pub verification: Option<VerificationStatus>,
    /// Optional implementation that produced a specialized edit.
    pub producer: Option<EditProducer>,
}

/// Immutable tracked-document state captured at the preview freshness
/// boundary. The synchronous planner never reads mutable tracker state.
#[derive(Debug, Default)]
pub(crate) struct PreviewDocuments(BTreeMap<PathBuf, DocumentSnapshot>);

/// Producer selected for a specialized edit preview.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditProducer {
    /// Workspace edit returned by the active language server.
    RustAnalyzer,
    /// MCPLS's in-process structural Rust refactor.
    StructuralAstGrep,
    /// Edits returned by `workspace/willRenameFiles` providers.
    LanguageServerFileOperations,
}

impl EditProducer {
    /// Return the stable wire value used by MCP responses.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RustAnalyzer => "rust_analyzer",
            Self::StructuralAstGrep => "structural_ast_grep",
            Self::LanguageServerFileOperations => "language_server_file_operations",
        }
    }
}

/// Semantic confidence attached to a specialized edit preview or application.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerificationStatus {
    /// No analyzer/compiler proof was available.
    StructuralUnverified,
    /// The project analyzer accepted the selected identity and post-apply checks passed.
    SemanticVerified,
    /// The edit committed, but post-apply semantic checks did not complete successfully.
    SemanticPostcheckFailed,
}

impl VerificationStatus {
    /// Return the stable wire value used by MCP responses.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::StructuralUnverified => "structural_unverified",
            Self::SemanticVerified => "semantic_verified",
            Self::SemanticPostcheckFailed => "semantic_postcheck_failed",
        }
    }
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
    /// A tracked document could not be refreshed before planning.
    #[error("failed to refresh workspace edit target {path}: {error}")]
    Refresh {
        /// Target path.
        path: PathBuf,
        /// Refresh failure.
        error: String,
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

/// Refresh every tracked document touched by a workspace edit before planning.
///
/// The tracker remains authoritative for clean open documents after external
/// rewrites, while dirty documents retain their in-memory text and conflict
/// state. Untracked files stay disk-owned and are read by the planner itself.
pub(crate) async fn refresh_workspace_edit_documents(
    edit: &WorkspaceEdit,
    documents: &DocumentTracker,
    limits: PreviewLimits,
) -> Result<PreviewDocuments, PreviewError> {
    let normalized = normalize(edit.clone()).expect("workspace edit normalization is infallible");
    let mut paths = BTreeSet::new();
    for operation in normalized.operations {
        match operation {
            EditOperation::Text { uri, .. }
            | EditOperation::Create { uri, .. }
            | EditOperation::Delete { uri, .. } => {
                paths.insert(path_for_uri(&uri.to_string())?);
            }
            EditOperation::Rename {
                old_uri, new_uri, ..
            } => {
                paths.insert(path_for_uri(&old_uri.to_string())?);
                paths.insert(path_for_uri(&new_uri.to_string())?);
            }
        }
        if paths.len() > limits.max_files {
            return Err(PreviewError::Limit {
                kind: "file",
                actual: paths.len(),
                limit: limits.max_files,
            });
        }
    }
    let mut snapshots = BTreeMap::new();
    for path in paths {
        if let Some(snapshot) = documents
            .reconciled_snapshot(&path)
            .await
            .map_err(|error| PreviewError::Refresh {
                path: path.clone(),
                error: error.to_string(),
            })?
        {
            snapshots.insert(path, snapshot);
        }
    }
    Ok(PreviewDocuments(snapshots))
}

struct PlannedFile {
    source: SnapshotSource,
    version: Option<i32>,
    created: bool,
    disk_matches: bool,
    original: String,
    planned: String,
}

/// Build a write-free plan from one LSP workspace edit.
///
/// Text edits are applied against the exact disk or open-document contents
/// observed during preview. Resource operations are path-validated and kept
/// as explicit operations for apply-time revalidation, so they can never
/// silently turn into arbitrary file writes.
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
pub(crate) fn preview_workspace_edit(
    boundary: &WorkspaceBoundary,
    project_id: &str,
    edit: WorkspaceEdit,
    encoding: PositionEncoding,
    documents: &PreviewDocuments,
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
    documents: &PreviewDocuments,
    limits: PreviewLimits,
) -> Result<PreviewArtifact, PreviewError> {
    PreviewBuilder::new(boundary, project_id, encoding, documents, limits).finish(normalized)
}

struct PreviewBuilder<'a> {
    boundary: &'a WorkspaceBoundary,
    project_id: &'a str,
    encoding: PositionEncoding,
    documents: &'a PreviewDocuments,
    limits: PreviewLimits,
    files: BTreeMap<PathBuf, PlannedFile>,
    created_paths: BTreeSet<PathBuf>,
    create_overwrites: BTreeMap<PathBuf, bool>,
    saw_text_edits: bool,
    saw_non_create_resource_operation: bool,
    file_operations: Vec<FileOperation>,
    operations: Vec<String>,
    conflicts: Vec<String>,
    unsupported: Vec<String>,
    edit_count: usize,
    resource_operation_count: usize,
}

impl<'a> PreviewBuilder<'a> {
    const fn new(
        boundary: &'a WorkspaceBoundary,
        project_id: &'a str,
        encoding: PositionEncoding,
        documents: &'a PreviewDocuments,
        limits: PreviewLimits,
    ) -> Self {
        Self {
            boundary,
            project_id,
            encoding,
            documents,
            limits,
            files: BTreeMap::new(),
            created_paths: BTreeSet::new(),
            create_overwrites: BTreeMap::new(),
            saw_text_edits: false,
            saw_non_create_resource_operation: false,
            file_operations: Vec::new(),
            operations: Vec::new(),
            conflicts: Vec::new(),
            unsupported: Vec::new(),
            edit_count: 0,
            resource_operation_count: 0,
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
        self.check_file_limits()?;
        if self.operations.is_empty() {
            self.conflicts
                .push("workspace edit contains no operations".to_string());
        }
        if !self.files.is_empty()
            && self
                .file_operations
                .iter()
                .any(|operation| matches!(operation, FileOperation::Delete { .. }))
        {
            self.conflicts
                .push("text edits cannot be combined with delete operations".to_string());
        }

        let snapshots = self
            .files
            .into_iter()
            .map(|(path, file)| {
                if file.created {
                    FileSnapshot::from_created_contents(path, file.planned)
                } else {
                    FileSnapshot::from_contents(
                        path,
                        file.source,
                        file.version,
                        file.original,
                        file.planned,
                    )
                }
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
        )
        .with_workspace_root(self.boundary.root().to_path_buf())
        .with_file_operations(self.file_operations);
        Ok(PreviewArtifact {
            plan,
            affected_files,
            conflicts: self.conflicts,
            unsupported: self.unsupported,
            verification: None,
            producer: None,
        })
    }

    #[allow(clippy::too_many_lines)]
    fn handle_operation(&mut self, operation: EditOperation) -> Result<(), PreviewError> {
        match operation {
            EditOperation::Text {
                uri,
                version,
                edits,
            } => self.handle_text(&uri.to_string(), version, &edits),
            EditOperation::Create { uri, options, .. } => {
                self.count_resource_operation()?;
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
                if self.saw_text_edits {
                    self.conflicts.push(format!(
                        "create {} follows text edits and cannot be transactionally ordered",
                        path.display()
                    ));
                }
                if self.created_paths.contains(&path) {
                    self.conflicts
                        .push(format!("duplicate create operation: {}", path.display()));
                }
                let operation = format!("create {}", path.display());
                self.operations.push(operation);
                self.created_paths.insert(path.clone());
                self.create_overwrites.insert(path.clone(), overwrite);
                self.file_operations
                    .push(FileOperation::Create { path, overwrite });
                if options
                    .as_ref()
                    .and_then(|options| options.ignore_if_exists)
                    .unwrap_or(false)
                {
                    self.unsupported
                        .push("create ignoreIfExists is not supported".to_string());
                }
                Ok(())
            }
            EditOperation::Rename {
                old_uri,
                new_uri,
                options,
                ..
            } => {
                self.saw_non_create_resource_operation = true;
                self.count_resource_operation()?;
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
                self.operations.push(operation);
                self.file_operations.push(FileOperation::Rename {
                    from,
                    to,
                    overwrite,
                });
                if options
                    .as_ref()
                    .and_then(|options| options.ignore_if_exists)
                    .unwrap_or(false)
                {
                    self.unsupported
                        .push("rename ignoreIfExists is not supported".to_string());
                }
                Ok(())
            }
            EditOperation::Delete { uri, options, .. } => {
                self.saw_non_create_resource_operation = true;
                self.count_resource_operation()?;
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
                self.operations.push(operation);
                self.file_operations
                    .push(FileOperation::Delete { path, recursive });
                if options
                    .as_ref()
                    .and_then(|options| options.ignore_if_not_exists)
                    .unwrap_or(false)
                {
                    self.unsupported
                        .push("delete ignoreIfNotExists is not supported".to_string());
                }
                Ok(())
            }
        }
    }

    const fn count_resource_operation(&mut self) -> Result<(), PreviewError> {
        self.resource_operation_count = self.resource_operation_count.saturating_add(1);
        if self.resource_operation_count > self.limits.max_resource_operations {
            return Err(PreviewError::Limit {
                kind: "resource operation",
                actual: self.resource_operation_count,
                limit: self.limits.max_resource_operations,
            });
        }
        Ok(())
    }

    fn check_file_limits(&self) -> Result<(), PreviewError> {
        for file in self.files.values() {
            let file_bytes = file.original.len().saturating_add(file.planned.len());
            if file_bytes > self.limits.max_file_bytes {
                return Err(PreviewError::Limit {
                    kind: "file byte",
                    actual: file_bytes,
                    limit: self.limits.max_file_bytes,
                });
            }
        }
        Ok(())
    }

    fn handle_text(
        &mut self,
        uri: &str,
        version: Option<i32>,
        edits: &[crate::workspace_edit::NormalizedTextEdit],
    ) -> Result<(), PreviewError> {
        if self.saw_non_create_resource_operation {
            self.conflicts
                .push("text edits follow a rename or delete operation".to_string());
        }
        self.saw_text_edits = true;
        self.edit_count = self.edit_count.saturating_add(edits.len());
        if self.edit_count > self.limits.max_edits {
            return Err(PreviewError::Limit {
                kind: "edit",
                actual: self.edit_count,
                limit: self.limits.max_edits,
            });
        }
        let requested_path = path_for_uri(uri)?;
        let path = if self.created_paths.contains(&requested_path) {
            self.boundary.validate_target(&requested_path)?
        } else {
            self.boundary.validate_existing(requested_path)?
        };
        if !self.files.contains_key(&path) {
            let file = if self.created_paths.contains(&path) {
                if self.create_overwrites.get(&path).copied().unwrap_or(false) {
                    let mut file = initial_file(&path, self.documents)?;
                    file.planned.clear();
                    file
                } else {
                    PlannedFile {
                        source: SnapshotSource::Disk,
                        version: None,
                        created: true,
                        disk_matches: true,
                        original: String::new(),
                        planned: String::new(),
                    }
                }
            } else {
                initial_file(&path, self.documents)?
            };
            if !file.disk_matches {
                self.conflicts.push(format!(
                    "open document differs from disk: {}",
                    path.display()
                ));
            }
            self.files.insert(path.clone(), file);
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
}

fn path_for_uri(uri: &str) -> Result<PathBuf, PreviewError> {
    let parsed = uri
        .parse::<lsp_types::Uri>()
        .map_err(|_| PreviewError::InvalidUri(uri.to_string()))?;
    uri_to_path(&parsed).ok_or_else(|| PreviewError::InvalidUri(uri.to_string()))
}

fn initial_file(path: &PathBuf, documents: &PreviewDocuments) -> Result<PlannedFile, PreviewError> {
    if let Some(document) = documents.0.get(path) {
        let disk = fs::read_to_string(path).map_err(|source| PreviewError::Read {
            path: path.clone(),
            source,
        })?;
        return Ok(PlannedFile {
            source: SnapshotSource::OpenDocument,
            version: Some(document.version()),
            created: false,
            disk_matches: disk == document.content(),
            original: document.content().to_string(),
            planned: document.content().to_string(),
        });
    }
    let original = fs::read_to_string(path).map_err(|source| PreviewError::Read {
        path: path.clone(),
        source,
    })?;
    Ok(PlannedFile {
        source: SnapshotSource::Disk,
        version: None,
        created: false,
        disk_matches: true,
        planned: original.clone(),
        original,
    })
}

#[cfg(test)]
#[path = "edit_preview_tests.rs"]
mod tests;
