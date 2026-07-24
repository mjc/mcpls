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

        let name = supplied
            .file_name()
            .ok_or_else(|| PathSafetyError::MissingParent(supplied.clone()))?;
        let parent = Self::nearest_existing_parent(&supplied)?;
        let canonical_parent = fs::canonicalize(&parent)
            .map_err(|_| PathSafetyError::MissingParent(parent.clone()))?;
        let canonical_parent = self.validate_canonical(canonical_parent)?;
        Ok(canonical_parent.join(name))
    }

    fn resolve_input(&self, path: &Path) -> PathBuf {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.root.join(path)
        }
    }

    fn nearest_existing_parent(path: &Path) -> Result<PathBuf, PathSafetyError> {
        let mut current = path.to_path_buf();
        while !current.exists() {
            let Some(parent) = current.parent() else {
                return Err(PathSafetyError::MissingParent(path.to_path_buf()));
            };
            if parent == current {
                return Err(PathSafetyError::MissingParent(path.to_path_buf()));
            }
            current = parent.to_path_buf();
        }
        Ok(current)
    }

    fn validate_canonical(&self, canonical: PathBuf) -> Result<PathBuf, PathSafetyError> {
        if !canonical.starts_with(&self.root) {
            return Err(PathSafetyError::OutsideWorkspace {
                root: self.root.clone(),
                path: canonical,
            });
        }
        if canonical == self.root {
            return Err(PathSafetyError::RootTarget(canonical));
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
}
