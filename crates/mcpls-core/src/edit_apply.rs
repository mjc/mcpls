//! Prevalidated application of edit-plan file snapshots.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::bridge::DocumentTracker;
use crate::edit_paths::PathSafetyError;
use crate::edit_paths::WorkspaceBoundary;
use crate::edit_plan::{
    EditPlan, EditPlanStore, FileSnapshot, PlanId, PlanStoreError, SnapshotSource,
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
    apply_plan_internal(boundary, plan, None)
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
    apply_plan_internal(boundary, plan, Some(documents))
}

fn apply_plan_internal(
    boundary: &WorkspaceBoundary,
    plan: &EditPlan,
    documents: Option<&DocumentTracker>,
) -> Result<ApplyReport, ApplyError> {
    let prepared = PreparedPlan::new(boundary, plan, documents)?;
    let staged = prepared.stage()?;
    prepared.revalidate(boundary, &staged, documents)?;
    PreparedPlan::commit(&staged)
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
    let plan = store.take_for_project(plan_id, project_id)?;
    apply_plan(boundary, &plan)
}

struct PreparedPlan<'a> {
    plan: &'a EditPlan,
    snapshots: Vec<&'a FileSnapshot>,
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

        let snapshots: Vec<_> = plan
            .files()
            .iter()
            .map(|snapshot| validate_snapshot(boundary, snapshot, documents))
            .collect::<Result<_, _>>()?;
        Ok(Self { plan, snapshots })
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
            if let Err(error) = validate_snapshot(boundary, snapshot, documents) {
                cleanup_staged(staged);
                return Err(error);
            }
        }
        Ok(())
    }

    fn commit(staged: &[StagedFile]) -> Result<ApplyReport, ApplyError> {
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

        Ok(ApplyReport { committed_files })
    }
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
) -> Result<&'a FileSnapshot, ApplyError> {
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
    match snapshot.source() {
        SnapshotSource::Disk => validate_disk_snapshot(snapshot)?,
        SnapshotSource::OpenDocument => validate_open_document_snapshot(snapshot, documents)?,
    }
    Ok(snapshot)
}

fn validate_disk_snapshot(snapshot: &FileSnapshot) -> Result<(), ApplyError> {
    let current = fs::read_to_string(snapshot.path()).map_err(|source| ApplyError::Stage {
        path: snapshot.path().clone(),
        source,
    })?;
    snapshot.validate(&current, None)?;
    Ok(())
}

fn validate_open_document_snapshot(
    snapshot: &FileSnapshot,
    documents: Option<&DocumentTracker>,
) -> Result<(), ApplyError> {
    let Some(documents) = documents else {
        return Err(ApplyError::UnsupportedSource(snapshot.path().clone()));
    };
    let Some(document) = documents.get(snapshot.path()) else {
        return Err(ApplyError::Stale(SnapshotValidationError::VersionChanged {
            path: snapshot.path().clone(),
            expected: snapshot.version().unwrap_or_default(),
            actual: None,
        }));
    };
    snapshot.validate(&document.content, Some(document.version))?;
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
#[allow(clippy::unwrap_used)]
mod tests {
    use std::collections::HashMap;
    use std::fs;
    use std::time::Duration;

    use tempfile::TempDir;

    use super::*;
    use crate::bridge::{DocumentTracker, ResourceLimits};
    use crate::edit_plan::{EditPlan, FileSnapshot, SnapshotSource};

    #[test]
    fn applies_all_disk_snapshots_after_revalidating_preconditions() {
        let root = TempDir::new().unwrap();
        let first = root.path().join("first.rs");
        let second = root.path().join("second.rs");
        fs::write(&first, "first\n").unwrap();
        fs::write(&second, "second\n").unwrap();
        let plan = EditPlan::new(
            "project".to_string(),
            vec![
                FileSnapshot::from_contents(
                    first.clone(),
                    SnapshotSource::Disk,
                    None,
                    "first\n",
                    "updated first\n",
                ),
                FileSnapshot::from_contents(
                    second.clone(),
                    SnapshotSource::Disk,
                    None,
                    "second\n",
                    "updated second\n",
                ),
            ],
            Vec::new(),
            true,
            Duration::from_secs(60),
        );
        let boundary = WorkspaceBoundary::new(root.path()).unwrap();

        let report = apply_plan(&boundary, &plan).unwrap();

        assert_eq!(report.committed_files, vec![first, second]);
        assert_eq!(
            fs::read_to_string(root.path().join("first.rs")).unwrap(),
            "updated first\n"
        );
        assert_eq!(
            fs::read_to_string(root.path().join("second.rs")).unwrap(),
            "updated second\n"
        );
    }

    #[test]
    fn rejects_one_stale_snapshot_before_committing_any_file() {
        let root = TempDir::new().unwrap();
        let first = root.path().join("first.rs");
        let second = root.path().join("second.rs");
        fs::write(&first, "first\n").unwrap();
        fs::write(&second, "second\n").unwrap();
        let plan = EditPlan::new(
            "project".to_string(),
            vec![
                FileSnapshot::from_contents(
                    first.clone(),
                    SnapshotSource::Disk,
                    None,
                    "first\n",
                    "updated first\n",
                ),
                FileSnapshot::from_contents(
                    second.clone(),
                    SnapshotSource::Disk,
                    None,
                    "second\n",
                    "updated second\n",
                ),
            ],
            Vec::new(),
            true,
            Duration::from_secs(60),
        );
        let boundary = WorkspaceBoundary::new(root.path()).unwrap();
        fs::write(&second, "changed after preview\n").unwrap();

        assert!(matches!(
            apply_plan(&boundary, &plan),
            Err(ApplyError::Stale(_))
        ));
        assert_eq!(fs::read_to_string(first).unwrap(), "first\n");
    }

    #[test]
    fn rejects_open_document_snapshots_without_disk_writes() {
        let root = TempDir::new().unwrap();
        let file = root.path().join("open.rs");
        fs::write(&file, "disk\n").unwrap();
        let plan = EditPlan::new(
            "project".to_string(),
            vec![FileSnapshot::from_contents(
                file.clone(),
                SnapshotSource::OpenDocument,
                Some(3),
                "dirty\n",
                "updated\n",
            )],
            Vec::new(),
            true,
            Duration::from_secs(60),
        );
        let boundary = WorkspaceBoundary::new(root.path()).unwrap();

        assert!(matches!(
            apply_plan(&boundary, &plan),
            Err(ApplyError::UnsupportedSource(path)) if path == file
        ));
        assert_eq!(fs::read_to_string(file).unwrap(), "disk\n");
    }

    #[test]
    fn applies_open_document_snapshot_against_tracked_content() {
        let root = TempDir::new().unwrap();
        let file = root.path().join("open.rs");
        fs::write(&file, "disk\n").unwrap();
        let mut documents = DocumentTracker::new(ResourceLimits::default(), HashMap::new());
        documents.open(file.clone(), "dirty\n".to_string()).unwrap();
        let plan = EditPlan::new(
            "project".to_string(),
            vec![FileSnapshot::from_contents(
                file.clone(),
                SnapshotSource::OpenDocument,
                Some(1),
                "dirty\n",
                "updated\n",
            )],
            Vec::new(),
            true,
            Duration::from_secs(60),
        );
        let boundary = WorkspaceBoundary::new(root.path()).unwrap();

        apply_plan_with_documents(&boundary, &plan, &documents).unwrap();

        assert_eq!(fs::read_to_string(file).unwrap(), "updated\n");
    }

    #[test]
    fn stored_application_consumes_the_plan_before_effects() {
        let root = TempDir::new().unwrap();
        let file = root.path().join("stored.rs");
        fs::write(&file, "before\n").unwrap();
        let plan = EditPlan::new(
            "project".to_string(),
            vec![FileSnapshot::from_contents(
                file.clone(),
                SnapshotSource::Disk,
                None,
                "before\n",
                "after\n",
            )],
            Vec::new(),
            true,
            Duration::from_secs(60),
        );
        let plan_id = plan.id().clone();
        let mut store = EditPlanStore::new(2, 1024, Duration::from_secs(60));
        store.insert(plan).unwrap();
        let boundary = WorkspaceBoundary::new(root.path()).unwrap();

        apply_stored_plan(&mut store, &boundary, "project", &plan_id).unwrap();

        assert_eq!(fs::read_to_string(&file).unwrap(), "after\n");
        assert!(matches!(
            apply_stored_plan(&mut store, &boundary, "project", &plan_id),
            Err(ApplyError::Store(PlanStoreError::NotFound(_)))
        ));
    }
}
