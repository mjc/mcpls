//! Prevalidated application of edit-plan file snapshots.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::edit_paths::PathSafetyError;
use crate::edit_paths::WorkspaceBoundary;
use crate::edit_plan::{EditPlan, FileSnapshot, SnapshotSource, SnapshotValidationError};

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
    let prepared = PreparedPlan::new(boundary, plan)?;
    let staged = prepared.stage()?;
    prepared.revalidate(boundary, &staged)?;
    PreparedPlan::commit(&staged)
}

struct PreparedPlan<'a> {
    plan: &'a EditPlan,
    snapshots: Vec<&'a FileSnapshot>,
}

impl<'a> PreparedPlan<'a> {
    fn new(boundary: &WorkspaceBoundary, plan: &'a EditPlan) -> Result<Self, ApplyError> {
        if !plan.safe_to_apply() {
            return Err(ApplyError::UnsafePlan);
        }
        if plan.is_expired(SystemTime::now()) {
            return Err(ApplyError::Expired);
        }

        let snapshots: Vec<_> = plan
            .files()
            .iter()
            .map(|snapshot| validate_snapshot(boundary, snapshot))
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
    ) -> Result<(), ApplyError> {
        // Revalidate after staging and immediately before the first
        // destructive operation. This rejects a stale plan as one unit.
        for snapshot in &self.snapshots {
            if let Err(error) = validate_snapshot(boundary, snapshot) {
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
) -> Result<&'a FileSnapshot, ApplyError> {
    if snapshot.source() != SnapshotSource::Disk {
        return Err(ApplyError::UnsupportedSource(snapshot.path().clone()));
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
    let current = fs::read_to_string(snapshot.path()).map_err(|source| ApplyError::Stage {
        path: snapshot.path().clone(),
        source,
    })?;
    snapshot.validate(&current, None)?;
    Ok(snapshot)
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
    use std::fs;
    use std::time::Duration;

    use tempfile::TempDir;

    use super::*;
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
}
