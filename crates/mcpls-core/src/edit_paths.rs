//! Canonical workspace containment for planned file operations.

use std::fs;
use std::path::{Path, PathBuf};

/// Errors raised while validating a workspace path before edit application.
#[derive(Debug, thiserror::Error)]
pub enum PathSafetyError {
    /// The registered root could not be canonicalized.
    #[error("workspace root is unavailable: {path}: {source}")]
    RootUnavailable {
        /// Root path supplied by the caller.
        path: PathBuf,
        /// Filesystem error.
        #[source]
        source: std::io::Error,
    },
    /// A path resolves outside the registered canonical root.
    #[error("path escapes workspace {root}: {path}")]
    OutsideWorkspace {
        /// Registered canonical root.
        root: PathBuf,
        /// Resolved path.
        path: PathBuf,
    },
    /// A target or one of its parents does not exist yet.
    #[error("path has no existing canonical parent: {0}")]
    MissingParent(PathBuf),
    /// An operation targeted a special file type.
    #[error("special file is not an editable workspace path: {0}")]
    SpecialFile(PathBuf),
    /// The operation targeted the workspace root itself.
    #[error("workspace root cannot be used as a file operation target: {0}")]
    RootTarget(PathBuf),
}

/// A filesystem operation that can be validated before it is applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileOperation {
    /// Create a file or directory at `path`.
    Create {
        /// Destination path.
        path: PathBuf,
        /// Permit replacing an existing destination.
        overwrite: bool,
    },
    /// Move an existing path to `to`.
    Rename {
        /// Existing source path.
        from: PathBuf,
        /// Destination path.
        to: PathBuf,
        /// Permit replacing an existing destination.
        overwrite: bool,
    },
    /// Delete an existing path.
    Delete {
        /// Path to remove.
        path: PathBuf,
        /// Permit deleting a directory recursively.
        recursive: bool,
    },
}

/// Errors raised when a file operation fails its preconditions.
#[derive(Debug, thiserror::Error)]
pub enum OperationValidationError {
    /// One of the operation paths crossed the workspace boundary.
    #[error("unsafe operation path: {0}")]
    Path(#[from] PathSafetyError),
    /// A destination already exists and overwrite was not enabled.
    #[error("operation destination already exists: {0}")]
    DestinationExists(PathBuf),
    /// A directory delete requires the recursive flag.
    #[error("directory delete requires recursive=true: {0}")]
    RecursiveDeleteRequired(PathBuf),
}

/// Canonical paths for an operation that passed validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidatedFileOperation {
    /// Validated create destination.
    Create {
        /// Canonical destination path.
        path: PathBuf,
        /// Whether an existing destination may be replaced.
        overwrite: bool,
    },
    /// Validated rename source and destination.
    Rename {
        /// Canonical source path.
        from: PathBuf,
        /// Canonical destination path.
        to: PathBuf,
        /// Whether an existing destination may be replaced.
        overwrite: bool,
    },
    /// Validated delete target.
    Delete {
        /// Canonical target path.
        path: PathBuf,
        /// Whether recursive deletion is allowed.
        recursive: bool,
    },
}

/// Canonical workspace boundary used by preview and apply validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceBoundary {
    root: PathBuf,
}

impl WorkspaceBoundary {
    /// Canonicalize and retain one registered workspace root.
    ///
    /// # Errors
    ///
    /// Returns [`PathSafetyError::RootUnavailable`] when the root does not
    /// exist or cannot be canonicalized.
    pub fn new(root: impl AsRef<Path>) -> Result<Self, PathSafetyError> {
        let supplied = root.as_ref().to_path_buf();
        let root =
            fs::canonicalize(&supplied).map_err(|source| PathSafetyError::RootUnavailable {
                path: supplied,
                source,
            })?;
        if !root.is_dir() {
            return Err(PathSafetyError::RootUnavailable {
                path: root,
                source: std::io::Error::new(
                    std::io::ErrorKind::NotADirectory,
                    "workspace root is not a directory",
                ),
            });
        }
        Ok(Self { root })
    }

    /// Return the registered canonical root.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Validate an existing path, resolving every symlink component.
    ///
    /// # Errors
    ///
    /// Returns an error when the path is missing, escapes the root, targets
    /// the root itself, or resolves to a special file.
    pub fn validate_existing(&self, path: impl AsRef<Path>) -> Result<PathBuf, PathSafetyError> {
        let supplied = self.resolve_input(path.as_ref());
        let canonical = fs::canonicalize(&supplied)
            .map_err(|_| PathSafetyError::MissingParent(supplied.clone()))?;
        self.validate_canonical(canonical)
    }

    /// Validate a file-operation target, including a not-yet-created target.
    ///
    /// # Errors
    ///
    /// Returns an error when an existing component escapes the root, the
    /// target has no existing parent, or a parent is a special file.
    pub fn validate_target(&self, path: impl AsRef<Path>) -> Result<PathBuf, PathSafetyError> {
        let supplied = self.resolve_input(path.as_ref());
        if supplied == self.root {
            return Err(PathSafetyError::RootTarget(supplied));
        }
        if supplied.exists() {
            return self.validate_existing(supplied);
        }

        let (parent, missing_components) = Self::nearest_existing_parent(&supplied)?;
        let canonical_parent = fs::canonicalize(&parent)
            .map_err(|_| PathSafetyError::MissingParent(parent.clone()))?;
        let canonical_parent = self.validate_canonical_parent(canonical_parent)?;
        Ok(missing_components
            .iter()
            .rev()
            .fold(canonical_parent, |path, component| path.join(component)))
    }

    /// Validate all paths and preconditions for one file operation.
    ///
    /// # Errors
    ///
    /// Returns an error when a path leaves the workspace or an operation
    /// precondition such as overwrite or recursive deletion is not met.
    pub fn validate_operation(
        &self,
        operation: &FileOperation,
    ) -> Result<ValidatedFileOperation, OperationValidationError> {
        match operation {
            FileOperation::Create { path, overwrite } => {
                let path = self.validate_target(path)?;
                if path.exists() && !overwrite {
                    return Err(OperationValidationError::DestinationExists(path));
                }
                Ok(ValidatedFileOperation::Create {
                    path,
                    overwrite: *overwrite,
                })
            }
            FileOperation::Rename {
                from,
                to,
                overwrite,
            } => {
                let from = self.validate_existing(from)?;
                let to = self.validate_target(to)?;
                if to.exists() && !overwrite {
                    return Err(OperationValidationError::DestinationExists(to));
                }
                Ok(ValidatedFileOperation::Rename {
                    from,
                    to,
                    overwrite: *overwrite,
                })
            }
            FileOperation::Delete { path, recursive } => {
                let path = self.validate_existing(path)?;
                if path.is_dir() && !recursive {
                    return Err(OperationValidationError::RecursiveDeleteRequired(path));
                }
                Ok(ValidatedFileOperation::Delete {
                    path,
                    recursive: *recursive,
                })
            }
        }
    }

    /// Validate every operation in a plan without performing filesystem I/O
    /// beyond the required metadata and canonicalization checks.
    ///
    /// The returned vector preserves the input order, allowing callers to
    /// apply only the exact sequence that passed the same boundary check.
    ///
    /// # Errors
    ///
    /// Returns the first path or precondition error encountered.
    pub fn validate_operations(
        &self,
        operations: &[FileOperation],
    ) -> Result<Vec<ValidatedFileOperation>, OperationValidationError> {
        operations
            .iter()
            .map(|operation| self.validate_operation(operation))
            .collect()
    }

    fn resolve_input(&self, path: &Path) -> PathBuf {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.root.join(path)
        }
    }

    fn nearest_existing_parent(path: &Path) -> Result<(PathBuf, Vec<PathBuf>), PathSafetyError> {
        let mut current = path.to_path_buf();
        let mut missing_components = Vec::new();
        while !current.exists() {
            let name = current
                .file_name()
                .map(PathBuf::from)
                .ok_or_else(|| PathSafetyError::MissingParent(path.to_path_buf()))?;
            missing_components.push(name);
            let Some(parent) = current.parent() else {
                return Err(PathSafetyError::MissingParent(path.to_path_buf()));
            };
            if parent == current {
                return Err(PathSafetyError::MissingParent(path.to_path_buf()));
            }
            current = parent.to_path_buf();
        }
        Ok((current, missing_components))
    }

    fn validate_canonical(&self, canonical: PathBuf) -> Result<PathBuf, PathSafetyError> {
        let canonical = self.validate_contained(canonical)?;
        if canonical == self.root {
            return Err(PathSafetyError::RootTarget(canonical));
        }
        Ok(canonical)
    }

    fn validate_canonical_parent(&self, canonical: PathBuf) -> Result<PathBuf, PathSafetyError> {
        let canonical = self.validate_contained(canonical)?;
        if !canonical.is_dir() {
            return Err(PathSafetyError::SpecialFile(canonical));
        }
        Ok(canonical)
    }

    fn validate_contained(&self, canonical: PathBuf) -> Result<PathBuf, PathSafetyError> {
        if !canonical.starts_with(&self.root) {
            return Err(PathSafetyError::OutsideWorkspace {
                root: self.root.clone(),
                path: canonical,
            });
        }
        let metadata = fs::metadata(&canonical)
            .map_err(|_| PathSafetyError::MissingParent(canonical.clone()))?;
        if !metadata.is_file() && !metadata.is_dir() {
            return Err(PathSafetyError::SpecialFile(canonical));
        }
        Ok(canonical)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    #[test]
    fn validates_existing_files_and_rejects_escape_targets() {
        let root = TempDir::new().unwrap();
        let file = root.path().join("src.rs");
        fs::write(&file, "fn main() {}\n").unwrap();
        let boundary = WorkspaceBoundary::new(root.path()).unwrap();

        assert_eq!(
            boundary.validate_existing(&file).unwrap(),
            file.canonicalize().unwrap()
        );
        assert!(matches!(
            boundary.validate_target(root.path().join("../escape.rs")),
            Err(PathSafetyError::OutsideWorkspace { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_escape_for_existing_and_target_paths() {
        use std::os::unix::fs::symlink;

        let root = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let outside_file = outside.path().join("secret.rs");
        fs::write(&outside_file, "secret").unwrap();
        let link = root.path().join("link");
        symlink(outside.path(), &link).unwrap();
        let boundary = WorkspaceBoundary::new(root.path()).unwrap();

        assert!(matches!(
            boundary.validate_existing(link.join("secret.rs")),
            Err(PathSafetyError::OutsideWorkspace { .. })
        ));
        assert!(matches!(
            boundary.validate_target(link.join("new.rs")),
            Err(PathSafetyError::OutsideWorkspace { .. })
        ));
    }

    #[test]
    fn validates_operation_preconditions_after_containment() {
        let root = TempDir::new().unwrap();
        let source = root.path().join("source.rs");
        let existing = root.path().join("existing.rs");
        fs::write(&source, "source").unwrap();
        fs::write(&existing, "existing").unwrap();
        let boundary = WorkspaceBoundary::new(root.path()).unwrap();

        assert!(matches!(
            boundary.validate_operation(&FileOperation::Create {
                path: existing.clone(),
                overwrite: false,
            }),
            Err(OperationValidationError::DestinationExists(path)) if path == existing
        ));
        assert!(matches!(
            boundary.validate_operation(&FileOperation::Rename {
                from: source.clone(),
                to: existing.clone(),
                overwrite: true,
            }),
            Ok(ValidatedFileOperation::Rename { from, to, .. })
                if from == source && to == existing
        ));
    }

    #[test]
    fn preserves_nested_missing_target_components() {
        let root = TempDir::new().unwrap();
        let boundary = WorkspaceBoundary::new(root.path()).unwrap();
        let target = root.path().join("nested").join("new.txt");

        assert_eq!(boundary.validate_target(&target).unwrap(), target);
    }

    #[test]
    fn requires_recursive_directory_deletes() {
        let root = TempDir::new().unwrap();
        let directory = root.path().join("nested");
        fs::create_dir(&directory).unwrap();
        let boundary = WorkspaceBoundary::new(root.path()).unwrap();

        assert!(matches!(
            boundary.validate_operation(&FileOperation::Delete {
                path: directory.clone(),
                recursive: false,
            }),
            Err(OperationValidationError::RecursiveDeleteRequired(path)) if path == directory
        ));
    }

    #[test]
    fn validates_operation_batches_in_input_order() {
        let root = TempDir::new().unwrap();
        let first = root.path().join("first.rs");
        let second = root.path().join("second.rs");
        fs::write(&first, "first").unwrap();
        fs::write(&second, "second").unwrap();
        let boundary = WorkspaceBoundary::new(root.path()).unwrap();
        let operations = [
            FileOperation::Rename {
                from: first.clone(),
                to: root.path().join("renamed.rs"),
                overwrite: false,
            },
            FileOperation::Delete {
                path: second.clone(),
                recursive: false,
            },
        ];

        let validated = boundary.validate_operations(&operations).unwrap();

        assert!(matches!(
            &validated[..],
            [
                ValidatedFileOperation::Rename { from, .. },
                ValidatedFileOperation::Delete { path, .. }
            ] if from == &first && path == &second
        ));
    }
}
