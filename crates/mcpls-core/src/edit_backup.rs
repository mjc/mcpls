//! Optional, bounded backups for `WorkspaceEdit` plans.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::edit_paths::WorkspaceBoundary;
use crate::edit_plan::EditPlan;

/// Whether a backup failure blocks a write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackupFailureMode {
    /// Continue the requested edit when the optional backup cannot be made.
    FailOpen,
    /// Reject the requested edit when the backup cannot be made.
    FailClosed,
}

/// Bounded backup configuration rooted inside one workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupPolicy {
    root: PathBuf,
    max_archives: usize,
    max_bytes: usize,
    failure_mode: BackupFailureMode,
}

impl BackupPolicy {
    /// Validate and construct a backup policy for one workspace.
    ///
    /// # Errors
    ///
    /// Returns an error when limits are zero, the root escapes the workspace,
    /// or the root cannot be inspected.
    pub fn new(
        boundary: &WorkspaceBoundary,
        root: impl AsRef<Path>,
        max_archives: usize,
        max_bytes: usize,
        failure_mode: BackupFailureMode,
    ) -> Result<Self, BackupError> {
        if max_archives == 0 || max_bytes == 0 {
            return Err(BackupError::InvalidPolicy);
        }
        let supplied = root.as_ref();
        let candidate = if supplied.is_absolute() {
            supplied.to_path_buf()
        } else {
            boundary.root().join(supplied)
        };
        let root = if candidate.exists() {
            fs::canonicalize(&candidate).map_err(|source| BackupError::Io {
                path: candidate.clone(),
                source,
            })?
        } else {
            boundary
                .validate_target(&candidate)
                .map_err(|_| BackupError::outside(boundary, candidate.clone()))?
        };
        if root == boundary.root() || !root.starts_with(boundary.root()) {
            return Err(BackupError::outside(boundary, root));
        }
        Ok(Self {
            root,
            max_archives,
            max_bytes,
            failure_mode,
        })
    }

    /// Return the canonical backup root.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Return the configured failure behavior.
    #[must_use]
    pub const fn failure_mode(&self) -> BackupFailureMode {
        self.failure_mode
    }
}

/// Errors raised while creating or restoring a backup archive.
#[derive(Debug, thiserror::Error)]
pub enum BackupError {
    /// Backup limits are zero and cannot retain an archive.
    #[error("backup policy limits must be non-zero")]
    InvalidPolicy,
    /// The backup root is outside the workspace boundary.
    #[error("backup root escapes workspace {root}: {path}")]
    OutsideWorkspace {
        /// Workspace root.
        root: PathBuf,
        /// Rejected backup path.
        path: PathBuf,
    },
    /// A backup filesystem operation failed.
    #[error("backup I/O failed for {path}: {source}")]
    Io {
        /// Affected path.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: std::io::Error,
    },
    /// A backup manifest could not be encoded or decoded.
    #[error("backup manifest is invalid for {path}: {source}")]
    Manifest {
        /// Manifest path.
        path: PathBuf,
        /// JSON error.
        #[source]
        source: serde_json::Error,
    },
    /// The requested archive no longer exists.
    #[error("backup archive is missing: {0}")]
    Missing(PathBuf),
    /// A backup file changed or was truncated.
    #[error("backup contents are corrupt: {0}")]
    Corrupt(PathBuf),
}

impl BackupError {
    fn outside(boundary: &WorkspaceBoundary, path: PathBuf) -> Self {
        Self::OutsideWorkspace {
            root: boundary.root().to_path_buf(),
            path,
        }
    }
}

/// One bounded archive of a plan's original file contents.
#[derive(Debug, Clone)]
pub struct BackupArchive {
    directory: PathBuf,
    entries: Vec<BackupEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct BackupManifest {
    plan_id: String,
    entries: Vec<BackupEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct BackupEntry {
    target: PathBuf,
    file_name: String,
    bytes: usize,
}

impl BackupArchive {
    /// Capture the original contents of every disk snapshot in a plan.
    ///
    /// # Errors
    ///
    /// Returns an error when the archive root, target paths, or manifest
    /// cannot be written safely.
    pub fn create(
        policy: &BackupPolicy,
        boundary: &WorkspaceBoundary,
        plan: &EditPlan,
    ) -> Result<Self, BackupError> {
        ensure_root(policy, boundary)?;
        let directory = policy.root.join(plan.id().as_str());
        fs::create_dir(&directory).map_err(|source| BackupError::Io {
            path: directory.clone(),
            source,
        })?;
        let mut entries = Vec::new();
        for (index, snapshot) in plan.files().iter().enumerate() {
            let target = boundary
                .validate_existing(snapshot.path())
                .map_err(|_| BackupError::outside(boundary, snapshot.path().clone()))?;
            let file_name = format!("{index}.bak");
            let path = directory.join(&file_name);
            let bytes = snapshot.original_content().as_bytes();
            fs::write(&path, bytes).map_err(|source| BackupError::Io {
                path: path.clone(),
                source,
            })?;
            entries.push(BackupEntry {
                target,
                file_name,
                bytes: bytes.len(),
            });
        }
        let manifest = BackupManifest {
            plan_id: plan.id().as_str().to_owned(),
            entries: entries.clone(),
        };
        let manifest_path = directory.join("manifest.json");
        let encoded =
            serde_json::to_vec_pretty(&manifest).map_err(|source| BackupError::Manifest {
                path: manifest_path.clone(),
                source,
            })?;
        fs::write(&manifest_path, encoded).map_err(|source| BackupError::Io {
            path: manifest_path,
            source,
        })?;
        prune(policy)?;
        Ok(Self { directory, entries })
    }

    /// Restore the archived contents after revalidating every target path.
    ///
    /// # Errors
    ///
    /// Returns an error when the archive is missing, corrupt, or outside the
    /// supplied workspace boundary.
    pub fn restore(&self, boundary: &WorkspaceBoundary) -> Result<Vec<PathBuf>, BackupError> {
        if !self.directory.is_dir() {
            return Err(BackupError::Missing(self.directory.clone()));
        }
        let mut restored = Vec::with_capacity(self.entries.len());
        for entry in &self.entries {
            let target = boundary
                .validate_target(&entry.target)
                .map_err(|_| BackupError::outside(boundary, entry.target.clone()))?;
            let source = self.directory.join(&entry.file_name);
            let contents = fs::read(&source).map_err(|source_error| BackupError::Io {
                path: source.clone(),
                source: source_error,
            })?;
            if contents.len() != entry.bytes {
                return Err(BackupError::Corrupt(source));
            }
            fs::write(&target, contents).map_err(|source| BackupError::Io {
                path: target.clone(),
                source,
            })?;
            restored.push(target);
        }
        Ok(restored)
    }
}

fn ensure_root(policy: &BackupPolicy, boundary: &WorkspaceBoundary) -> Result<(), BackupError> {
    fs::create_dir_all(&policy.root).map_err(|source| BackupError::Io {
        path: policy.root.clone(),
        source,
    })?;
    let canonical = fs::canonicalize(&policy.root).map_err(|source| BackupError::Io {
        path: policy.root.clone(),
        source,
    })?;
    if canonical == boundary.root() || !canonical.starts_with(boundary.root()) {
        return Err(BackupError::outside(boundary, canonical));
    }
    Ok(())
}

fn prune(policy: &BackupPolicy) -> Result<(), BackupError> {
    let mut archives = fs::read_dir(&policy.root)
        .map_err(|source| BackupError::Io {
            path: policy.root.clone(),
            source,
        })?
        .filter_map(Result::ok)
        .filter(|entry| entry.path().is_dir())
        .map(|entry| {
            let path = entry.path();
            let modified = entry
                .metadata()
                .and_then(|metadata| metadata.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            let bytes = directory_bytes(&path);
            (path, modified, bytes)
        })
        .collect::<Vec<_>>();
    archives.sort_by_key(|(_, modified, _)| *modified);
    let mut total_bytes = archives.iter().map(|(_, _, bytes)| *bytes).sum::<usize>();
    while archives.len() > policy.max_archives || total_bytes > policy.max_bytes {
        let Some((path, _, bytes)) = archives.first().cloned() else {
            break;
        };
        fs::remove_dir_all(&path).map_err(|source| BackupError::Io {
            path: path.clone(),
            source,
        })?;
        archives.remove(0);
        total_bytes = total_bytes.saturating_sub(bytes);
    }
    Ok(())
}

fn directory_bytes(path: &Path) -> usize {
    fs::read_dir(path)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|entry| {
            let path = entry.path();
            entry.metadata().map_or(0, |metadata| {
                if metadata.is_dir() {
                    directory_bytes(&path)
                } else {
                    usize::try_from(metadata.len()).unwrap_or(usize::MAX)
                }
            })
        })
        .sum()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::fs;
    use std::time::Duration;

    use tempfile::TempDir;

    use super::*;
    use crate::edit_paths::WorkspaceBoundary;
    use crate::edit_plan::{EditPlan, FileSnapshot, SnapshotSource};

    #[test]
    fn backup_policy_rejects_a_root_outside_the_workspace() {
        let workspace = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let boundary = WorkspaceBoundary::new(workspace.path()).unwrap();

        assert!(matches!(
            BackupPolicy::new(
                &boundary,
                outside.path().join("backups"),
                2,
                1024,
                BackupFailureMode::FailClosed,
            ),
            Err(BackupError::OutsideWorkspace { .. })
        ));
    }

    #[test]
    fn backup_archives_restore_and_retain_bounded_entries() {
        let workspace = TempDir::new().unwrap();
        let backup_root = workspace.path().join(".mcpls-backups");
        fs::create_dir(&backup_root).unwrap();
        let file = workspace.path().join("src.rs");
        fs::write(&file, "before\n").unwrap();
        let boundary = WorkspaceBoundary::new(workspace.path()).unwrap();
        let policy = BackupPolicy::new(
            &boundary,
            &backup_root,
            1,
            1024 * 1024,
            BackupFailureMode::FailClosed,
        )
        .unwrap();
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

        let first = BackupArchive::create(&policy, &boundary, &plan).unwrap();
        fs::write(&file, "changed\n").unwrap();
        first.restore(&boundary).unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), "before\n");

        let second_plan = EditPlan::new(
            "project".to_string(),
            vec![FileSnapshot::from_contents(
                file,
                SnapshotSource::Disk,
                None,
                "before\n",
                "again\n",
            )],
            Vec::new(),
            true,
            Duration::from_secs(60),
        );
        BackupArchive::create(&policy, &boundary, &second_plan).unwrap();
        let archives = fs::read_dir(&backup_root).unwrap().count();
        assert_eq!(archives, 1);
    }
}
