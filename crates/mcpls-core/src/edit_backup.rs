//! Optional, bounded backups for `WorkspaceEdit` plans.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::edit_paths::{FileOperation, WorkspaceBoundary};
use crate::edit_plan::EditPlan;

/// Whether a backup failure blocks a write.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackupFailureMode {
    /// Continue the requested edit when the optional backup cannot be made.
    FailOpen,
    /// Reject the requested edit when the backup cannot be made.
    #[default]
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
    /// A resource operation targets a directory or special file that this
    /// bounded file archive cannot restore safely.
    #[error("backup does not support resource path: {0}")]
    Unsupported(PathBuf),
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
    file_name: Option<String>,
    kind: BackupEntryKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
enum BackupEntryKind {
    File { bytes: usize },
    Absent,
}

impl BackupArchive {
    /// Capture original file snapshots and resource-operation preimages.
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
                file_name: Some(file_name),
                kind: BackupEntryKind::File { bytes: bytes.len() },
            });
        }
        let mut next_file = entries.len();
        for operation in plan.file_operations() {
            match operation {
                FileOperation::Create { path, .. } => {
                    let target = boundary
                        .validate_target(path)
                        .map_err(|_| BackupError::outside(boundary, path.clone()))?;
                    entries.push(capture_target(&directory, &mut next_file, target)?);
                }
                FileOperation::Rename { from, to, .. } => {
                    let from = boundary
                        .validate_existing(from)
                        .map_err(|_| BackupError::outside(boundary, from.clone()))?;
                    entries.push(capture_existing(&directory, &mut next_file, from)?);
                    let to = boundary
                        .validate_target(to)
                        .map_err(|_| BackupError::outside(boundary, to.clone()))?;
                    entries.push(capture_target(&directory, &mut next_file, to)?);
                }
                FileOperation::Delete { path, .. } => {
                    let target = boundary
                        .validate_existing(path)
                        .map_err(|_| BackupError::outside(boundary, path.clone()))?;
                    entries.push(capture_existing(&directory, &mut next_file, target)?);
                }
            }
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
        for entry in self.entries.iter().rev() {
            let target = boundary
                .validate_target(&entry.target)
                .map_err(|_| BackupError::outside(boundary, entry.target.clone()))?;
            match (&entry.kind, &entry.file_name) {
                (BackupEntryKind::Absent, None) => {
                    if target.exists() {
                        if target.is_dir() {
                            fs::remove_dir_all(&target).map_err(|source| BackupError::Io {
                                path: target.clone(),
                                source,
                            })?;
                        } else {
                            fs::remove_file(&target).map_err(|source| BackupError::Io {
                                path: target.clone(),
                                source,
                            })?;
                        }
                    }
                }
                (BackupEntryKind::File { bytes }, Some(file_name)) => {
                    let source = self.directory.join(file_name);
                    let contents = fs::read(&source).map_err(|source_error| BackupError::Io {
                        path: source.clone(),
                        source: source_error,
                    })?;
                    if contents.len() != *bytes {
                        return Err(BackupError::Corrupt(source));
                    }
                    fs::write(&target, contents).map_err(|source| BackupError::Io {
                        path: target.clone(),
                        source,
                    })?;
                }
                _ => return Err(BackupError::Corrupt(entry.target.clone())),
            }
            restored.push(target);
        }
        Ok(restored)
    }
}

fn capture_target(
    directory: &Path,
    next_file: &mut usize,
    target: PathBuf,
) -> Result<BackupEntry, BackupError> {
    if target.exists() {
        capture_existing(directory, next_file, target)
    } else {
        Ok(BackupEntry {
            target,
            file_name: None,
            kind: BackupEntryKind::Absent,
        })
    }
}

fn capture_existing(
    directory: &Path,
    next_file: &mut usize,
    target: PathBuf,
) -> Result<BackupEntry, BackupError> {
    if !target.is_file() {
        return Err(BackupError::Unsupported(target));
    }
    let contents = fs::read(&target).map_err(|source| BackupError::Io {
        path: target.clone(),
        source,
    })?;
    let file_name = format!("{next_file}.bak");
    *next_file = next_file.saturating_add(1);
    let path = directory.join(&file_name);
    fs::write(&path, &contents).map_err(|source| BackupError::Io {
        path: path.clone(),
        source,
    })?;
    Ok(BackupEntry {
        target,
        file_name: Some(file_name),
        kind: BackupEntryKind::File {
            bytes: contents.len(),
        },
    })
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
#[path = "edit_backup_tests.rs"]
mod tests;
