//! Prevalidated application of edit-plan file snapshots.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[cfg(test)]
use std::collections::HashSet;
#[cfg(test)]
use std::sync::{Arc, Barrier, Mutex, OnceLock};

use crate::bridge::DocumentTracker;
use crate::edit_backup::{BackupArchive, BackupError, BackupFailureMode, BackupPolicy};
use crate::edit_paths::PathSafetyError;
use crate::edit_paths::{OperationValidationError, ValidatedFileOperation, WorkspaceBoundary};
use crate::edit_plan::{
    EditAuditRecord, EditPlan, EditPlanStore, FileSnapshot, PlanId, PlanStoreError, SnapshotSource,
    SnapshotValidationError,
};

/// Result of applying one edit plan.
#[derive(Debug, PartialEq, Eq)]
pub struct ApplyReport {
    /// Files replaced successfully.
    pub committed_files: Vec<PathBuf>,
}

/// Apply a disk-backed plan after revalidating its workspace boundary.
///
/// All files are checked before any target is replaced. Planned content is
/// staged beside its target, then each regular file is atomically replaced.
///
/// # Errors
///
/// Returns an error when the plan is stale, expired, unsafe, unsupported, or
/// when staging/commit I/O fails.
pub fn apply_plan(
    boundary: &WorkspaceBoundary,
    plan: &EditPlan,
) -> Result<ApplyReport, ApplyError> {
    apply_plan_internal(boundary, plan, None, None)
}

/// Apply a plan while validating open-document snapshots against the
/// project's in-memory document tracker.
///
/// # Errors
///
/// Returns the same validation and filesystem errors as [`apply_plan`], and
/// rejects an open-document snapshot when its tracked content or version is
/// stale.
pub fn apply_plan_with_documents(
    boundary: &WorkspaceBoundary,
    plan: &EditPlan,
    documents: &DocumentTracker,
) -> Result<ApplyReport, ApplyError> {
    apply_plan_internal(boundary, plan, Some(documents), None)
}

/// Apply a plan with an optional bounded backup archive.
///
/// # Errors
///
/// Returns validation, filesystem, or fail-closed backup errors.
pub fn apply_plan_with_backup(
    boundary: &WorkspaceBoundary,
    plan: &EditPlan,
    policy: &BackupPolicy,
) -> Result<ApplyReport, ApplyError> {
    apply_plan_internal(boundary, plan, None, Some(policy))
}

/// Apply a plan with open-document validation and a bounded backup archive.
///
/// # Errors
///
/// Returns validation, filesystem, or fail-closed backup errors.
pub fn apply_plan_with_documents_and_backup(
    boundary: &WorkspaceBoundary,
    plan: &EditPlan,
    documents: &DocumentTracker,
    policy: &BackupPolicy,
) -> Result<ApplyReport, ApplyError> {
    apply_plan_internal(boundary, plan, Some(documents), Some(policy))
}

fn apply_plan_internal(
    boundary: &WorkspaceBoundary,
    plan: &EditPlan,
    documents: Option<&DocumentTracker>,
    backup_policy: Option<&BackupPolicy>,
) -> Result<ApplyReport, ApplyError> {
    wait_for_test_apply_barrier(plan);
    let prepared = PreparedPlan::new(boundary, plan, documents)?;
    prepare_backup(backup_policy, boundary, plan)?;
    let staged = prepared.stage()?;
    prepared.revalidate(boundary, &staged, documents)?;
    PreparedPlan::commit(&staged, &prepared.operations)
}

#[cfg(test)]
struct TestApplyBarrier {
    plan_ids: HashSet<String>,
    barrier: Arc<Barrier>,
}

#[cfg(test)]
static TEST_APPLY_BARRIER: OnceLock<Mutex<Option<TestApplyBarrier>>> = OnceLock::new();

#[cfg(test)]
pub(crate) struct TestApplyBarrierGuard;

#[cfg(test)]
pub(crate) fn install_test_apply_barrier(
    plan_ids: impl IntoIterator<Item = PlanId>,
    parties: usize,
) -> TestApplyBarrierGuard {
    let state = TEST_APPLY_BARRIER.get_or_init(|| Mutex::new(None));
    *state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(TestApplyBarrier {
        plan_ids: plan_ids
            .into_iter()
            .map(|plan_id| plan_id.as_str().to_owned())
            .collect(),
        barrier: Arc::new(Barrier::new(parties)),
    });
    TestApplyBarrierGuard
}

#[cfg(test)]
impl Drop for TestApplyBarrierGuard {
    fn drop(&mut self) {
        if let Some(state) = TEST_APPLY_BARRIER.get() {
            *state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        }
    }
}

#[cfg(test)]
fn wait_for_test_apply_barrier(plan: &EditPlan) {
    let barrier = TEST_APPLY_BARRIER.get().and_then(|state| {
        state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .filter(|test| test.plan_ids.contains(plan.id().as_str()))
            .map(|test| Arc::clone(&test.barrier))
    });
    if let Some(barrier) = barrier {
        barrier.wait();
    }
}

#[cfg(not(test))]
const fn wait_for_test_apply_barrier(_plan: &EditPlan) {}

fn prepare_backup(
    policy: Option<&BackupPolicy>,
    boundary: &WorkspaceBoundary,
    plan: &EditPlan,
) -> Result<(), ApplyError> {
    let Some(policy) = policy else {
        return Ok(());
    };
    match BackupArchive::create(policy, boundary, plan) {
        Ok(_) => Ok(()),
        Err(_error) if policy.failure_mode() == BackupFailureMode::FailOpen => Ok(()),
        Err(error) => Err(ApplyError::Backup(error)),
    }
}

/// Consume and apply a plan from its owning project store.
///
/// # Errors
///
/// Returns a project/plan lookup error before applying, or any validation and
/// filesystem error returned by [`apply_plan`]. The plan is consumed before
/// the filesystem effect so it cannot be applied twice.
pub fn apply_stored_plan(
    store: &mut EditPlanStore,
    boundary: &WorkspaceBoundary,
    project_id: &str,
    plan_id: &PlanId,
) -> Result<ApplyReport, ApplyError> {
    apply_stored_plan_with_context(store, boundary, project_id, plan_id, None, None)
}

/// Consume and apply a stored plan while retaining optional caller context in
/// its audit record.
///
/// # Errors
///
/// Returns a plan lookup, application, or fail-closed audit error.
pub fn apply_stored_plan_with_context(
    store: &mut EditPlanStore,
    boundary: &WorkspaceBoundary,
    project_id: &str,
    plan_id: &PlanId,
    session_id: Option<String>,
    principal: Option<String>,
) -> Result<ApplyReport, ApplyError> {
    let plan = store.take_for_project(plan_id, project_id)?;
    let audit = EditAuditRecord::for_plan_with_context(&plan, session_id, principal);
    let result = apply_plan(boundary, &plan);
    let audit = match &result {
        Ok(report) => audit.committed(report.committed_files.clone()),
        Err(error) => audit.failed(error.to_string(), false),
    };
    let audit_result = store.record_audit_with_policy(audit);
    match (result, audit_result) {
        (Ok(report), Ok(())) => Ok(report),
        (Ok(_), Err(error)) => Err(ApplyError::Audit(error)),
        (Err(error), _) => Err(error),
    }
}

struct PreparedPlan<'a> {
    plan: &'a EditPlan,
    snapshots: Vec<&'a FileSnapshot>,
    operations: Vec<ValidatedFileOperation>,
}

impl<'a> PreparedPlan<'a> {
    fn new(
        boundary: &WorkspaceBoundary,
        plan: &'a EditPlan,
        documents: Option<&DocumentTracker>,
    ) -> Result<Self, ApplyError> {
        if !plan.safe_to_apply() {
            return Err(ApplyError::UnsafePlan);
        }
        if plan.is_expired(SystemTime::now()) {
            return Err(ApplyError::Expired);
        }

        let operations = boundary
            .validate_operations(plan.file_operations())
            .map_err(ApplyError::Operation)?;
        let snapshots: Vec<_> = plan
            .files()
            .iter()
            .map(|snapshot| validate_snapshot(boundary, snapshot, documents, &operations))
            .collect::<Result<_, _>>()?;
        Ok(Self {
            plan,
            snapshots,
            operations,
        })
    }

    fn stage(&self) -> Result<Vec<StagedFile>, ApplyError> {
        let mut staged = Vec::with_capacity(self.snapshots.len());
        for (index, snapshot) in self.snapshots.iter().enumerate() {
            let temp_path = temporary_path(snapshot.path(), self.plan, index);
            if let Err(error) = stage_file(&temp_path, snapshot) {
                cleanup_staged(&staged);
                return Err(ApplyError::Stage {
                    path: temp_path,
                    source: error,
                });
            }
            staged.push(StagedFile {
                target: snapshot.path().clone(),
                temp: temp_path,
            });
        }
        Ok(staged)
    }

    fn revalidate(
        &self,
        boundary: &WorkspaceBoundary,
        staged: &[StagedFile],
        documents: Option<&DocumentTracker>,
    ) -> Result<(), ApplyError> {
        // Revalidate after staging and immediately before the first
        // destructive operation. This rejects a stale plan as one unit.
        for snapshot in &self.snapshots {
            if let Err(error) = validate_snapshot(boundary, snapshot, documents, &self.operations) {
                cleanup_staged(staged);
                return Err(error);
            }
        }
        if let Err(error) = boundary.validate_operations(self.plan.file_operations()) {
            cleanup_staged(staged);
            return Err(ApplyError::Operation(error));
        }
        Ok(())
    }

    fn commit(
        staged: &[StagedFile],
        operations: &[ValidatedFileOperation],
    ) -> Result<ApplyReport, ApplyError> {
        let mut committed_files = commit_staged_files(staged)?;
        for operation in operations {
            let path = apply_resource_operation(operation, &committed_files)?;
            if !committed_files.contains(&path) {
                committed_files.push(path);
            }
        }

        Ok(ApplyReport { committed_files })
    }
}

fn commit_staged_files(staged: &[StagedFile]) -> Result<Vec<PathBuf>, ApplyError> {
    let mut committed_files = Vec::with_capacity(staged.len());
    for (index, file) in staged.iter().enumerate() {
        if let Err(error) = fs::rename(&file.temp, &file.target) {
            cleanup_staged(&staged[index..]);
            return Err(ApplyError::Commit {
                path: file.target.clone(),
                committed_files,
                source: error,
            });
        }
        committed_files.push(file.target.clone());
    }
    Ok(committed_files)
}

fn apply_resource_operation(
    operation: &ValidatedFileOperation,
    committed_files: &[PathBuf],
) -> Result<PathBuf, ApplyError> {
    match operation {
        ValidatedFileOperation::Create { path, overwrite } => {
            if committed_files.iter().any(|committed| committed == path) {
                return Ok(path.clone());
            }
            let result = if *overwrite {
                fs::write(path, [])
            } else {
                OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(path)
                    .map(|_| ())
            };
            result.map_err(|source| ApplyError::Resource {
                operation: format!("create {}", path.display()),
                committed_files: committed_files.to_vec(),
                source,
            })?;
            Ok(path.clone())
        }
        ValidatedFileOperation::Rename {
            from,
            to,
            overwrite,
        } => {
            ensure_destination_available(to, *overwrite)?;
            fs::rename(from, to).map_err(|source| ApplyError::Resource {
                operation: format!("rename {} -> {}", from.display(), to.display()),
                committed_files: committed_files.to_vec(),
                source,
            })?;
            Ok(to.clone())
        }
        ValidatedFileOperation::Delete { path, recursive } => {
            let result = if *recursive && path.is_dir() {
                fs::remove_dir_all(path)
            } else {
                fs::remove_file(path)
            };
            result.map_err(|source| ApplyError::Resource {
                operation: format!("delete {}", path.display()),
                committed_files: committed_files.to_vec(),
                source,
            })?;
            Ok(path.clone())
        }
    }
}

fn ensure_destination_available(path: &Path, overwrite: bool) -> Result<(), ApplyError> {
    if path.exists() && !overwrite {
        return Err(ApplyError::Operation(
            OperationValidationError::DestinationExists(path.to_path_buf()),
        ));
    }
    Ok(())
}

/// Errors returned while applying an edit plan.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ApplyError {
    /// The selected plan was not owned by the requested project or no longer
    /// exists in its bounded store.
    #[error("edit plan lookup failed: {0}")]
    Store(#[from] PlanStoreError),
    /// The plan cannot be applied safely.
    #[error("edit plan is not safe to apply")]
    UnsafePlan,
    /// A resource operation failed precondition validation.
    #[error("workspace edit resource operation is invalid: {0}")]
    Operation(#[from] OperationValidationError),
    /// The preview is no longer valid because its expiry was reached.
    #[error("edit plan has expired")]
    Expired,
    /// Open-document snapshots need actor-owned document state and are not
    /// silently written from disk.
    #[error("open-document snapshot cannot be applied by the disk applier: {0}")]
    UnsupportedSource(PathBuf),
    /// The target escaped or otherwise failed workspace validation.
    #[error("unsafe edit target {path}: {source}")]
    Path {
        /// Affected target.
        path: PathBuf,
        /// Boundary validation failure.
        #[source]
        source: PathSafetyError,
    },
    /// The target's canonical topology changed since preview.
    #[error("edit target topology changed: expected {expected}, got {actual}")]
    TopologyChanged {
        /// Path captured by the plan.
        expected: PathBuf,
        /// Current canonical path.
        actual: PathBuf,
    },
    /// The current content or version no longer matches the snapshot.
    #[error("edit snapshot is stale: {0}")]
    Stale(#[from] SnapshotValidationError),
    /// Staging failed before any target was replaced.
    #[error("failed to stage {path}: {source}")]
    Stage {
        /// Temporary staging path.
        path: PathBuf,
        /// Filesystem failure.
        #[source]
        source: std::io::Error,
    },
    /// A commit failed after zero or more prior replacements.
    #[error("failed to commit {path} after replacing {committed_files:?}: {source}")]
    Commit {
        /// Target that could not be replaced.
        path: PathBuf,
        /// Targets replaced before the failure.
        committed_files: Vec<PathBuf>,
        /// Filesystem failure.
        #[source]
        source: std::io::Error,
    },
    /// A resource operation failed after earlier edits committed.
    #[error("failed to apply {operation} after replacing {committed_files:?}: {source}")]
    Resource {
        /// Human-readable operation description.
        operation: String,
        /// Files already changed before the failure.
        committed_files: Vec<PathBuf>,
        /// Underlying filesystem error.
        #[source]
        source: std::io::Error,
    },
    /// A fail-closed backup could not be created before writing.
    #[error("failed to create edit backup: {0}")]
    Backup(#[source] BackupError),
    /// A fail-closed audit sink rejected a completed edit record.
    #[error("failed to record edit audit: {0}")]
    Audit(#[source] PlanStoreError),
}

#[derive(Debug)]
struct StagedFile {
    target: PathBuf,
    temp: PathBuf,
}

fn validate_snapshot<'a>(
    boundary: &WorkspaceBoundary,
    snapshot: &'a FileSnapshot,
    documents: Option<&DocumentTracker>,
    operations: &[ValidatedFileOperation],
) -> Result<&'a FileSnapshot, ApplyError> {
    if snapshot.was_created() {
        let canonical = boundary
            .validate_target(snapshot.path())
            .map_err(|source| ApplyError::Path {
                path: snapshot.path().clone(),
                source,
            })?;
        if canonical != *snapshot.path() {
            return Err(ApplyError::TopologyChanged {
                expected: snapshot.path().clone(),
                actual: canonical,
            });
        }
        if snapshot.path().exists()
            || operations.iter().any(|operation| {
                matches!(operation, ValidatedFileOperation::Create { path, .. } if path == snapshot.path() && path.exists())
            })
        {
            return Err(ApplyError::Operation(
                OperationValidationError::DestinationExists(snapshot.path().clone()),
            ));
        }
        return Ok(snapshot);
    }
    let canonical = boundary
        .validate_existing(snapshot.path())
        .map_err(|source| ApplyError::Path {
            path: snapshot.path().clone(),
            source,
        })?;
    if canonical != *snapshot.path() {
        return Err(ApplyError::TopologyChanged {
            expected: snapshot.path().clone(),
            actual: canonical,
        });
    }
    let is_open_document = snapshot.source() == SnapshotSource::OpenDocument;
    if is_open_document && documents.is_none() {
        return Err(ApplyError::UnsupportedSource(snapshot.path().clone()));
    }
    validate_disk_snapshot(snapshot)?;
    if is_open_document {
        validate_open_document_snapshot(snapshot, documents)?;
    }
    Ok(snapshot)
}

fn validate_disk_snapshot(snapshot: &FileSnapshot) -> Result<(), ApplyError> {
    let current = fs::read_to_string(snapshot.path()).map_err(|source| ApplyError::Stage {
        path: snapshot.path().clone(),
        source,
    })?;
    snapshot.validate_content(&current)?;
    Ok(())
}

fn validate_open_document_snapshot(
    snapshot: &FileSnapshot,
    documents: Option<&DocumentTracker>,
) -> Result<(), ApplyError> {
    let Some(documents) = documents else {
        return Err(ApplyError::UnsupportedSource(snapshot.path().clone()));
    };
    let Some(document) = documents.tracked_snapshot(snapshot.path()) else {
        return Err(ApplyError::Stale(SnapshotValidationError::VersionChanged {
            path: snapshot.path().clone(),
            expected: snapshot.version().unwrap_or_default(),
            actual: None,
        }));
    };
    snapshot.validate(document.content(), Some(document.version()))?;
    Ok(())
}

fn temporary_path(target: &Path, plan: &EditPlan, index: usize) -> PathBuf {
    target
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!(".mcpls-apply-{}-{index}.tmp", plan.id()))
}

fn stage_file(path: &Path, snapshot: &FileSnapshot) -> std::io::Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(snapshot.planned_content().as_bytes())?;
    file.sync_all()
}

fn cleanup_staged(files: &[StagedFile]) {
    for file in files {
        let _ = fs::remove_file(&file.temp);
    }
}

#[cfg(test)]
#[path = "edit_apply_tests.rs"]
mod tests;
