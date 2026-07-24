//! Project identity and canonical path routing primitives.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
/// Errors raised while constructing or routing project identities.
pub enum ProjectIdentityError {
    /// The supplied project ID contains no non-whitespace characters.
    #[error("project id must not be empty")]
    EmptyId,
    /// The supplied project root is not a directory.
    #[error("project root is not a directory: {path}")]
    RootNotDirectory {
        /// The path that was checked.
        path: PathBuf,
    },
    /// Canonicalization failed for a path.
    #[error("failed to canonicalize project path {path}: {source}")]
    Canonicalize {
        /// The path that could not be canonicalized.
        path: PathBuf,
        /// The underlying filesystem error.
        #[source]
        source: std::io::Error,
    },
    /// Two project identities use the same stable ID.
    #[error("duplicate project id: {0}")]
    DuplicateId(ProjectId),
    /// Two project identities use the same canonical root.
    #[error("duplicate project root: {0}")]
    DuplicateRoot(PathBuf),
    /// No registered project contains the requested path.
    #[error("path is not registered to a project: {0}")]
    UnregisteredPath(PathBuf),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
/// Stable identifier for a registered project.
pub struct ProjectId(String);

impl ProjectId {
    /// Create a project ID from a non-empty value.
    ///
    /// # Errors
    ///
    /// Returns [`ProjectIdentityError::EmptyId`] when the value is blank.
    pub fn new(value: impl Into<String>) -> Result<Self, ProjectIdentityError> {
        let value = value.into();
        (!value.trim().is_empty())
            .then_some(Self(value))
            .ok_or(ProjectIdentityError::EmptyId)
    }

    /// Return the stable ID value.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ProjectId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
/// Canonical, existing directory used as a project boundary.
pub struct CanonicalRoot(PathBuf);

impl CanonicalRoot {
    /// Canonicalize an existing directory and use it as a project root.
    ///
    /// # Errors
    ///
    /// Returns an error when the path cannot be canonicalized or is not a directory.
    pub fn new(path: impl AsRef<Path>) -> Result<Self, ProjectIdentityError> {
        let path = path.as_ref();
        let canonical =
            path.canonicalize()
                .map_err(|source| ProjectIdentityError::Canonicalize {
                    path: path.to_path_buf(),
                    source,
                })?;

        if canonical.is_dir() {
            Ok(Self(canonical))
        } else {
            Err(ProjectIdentityError::RootNotDirectory { path: canonical })
        }
    }

    /// Return the canonical root path.
    #[must_use]
    pub fn as_path(&self) -> &Path {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// Stable project ID paired with its canonical root.
pub struct ProjectIdentity {
    id: ProjectId,
    root: CanonicalRoot,
}

impl ProjectIdentity {
    /// Pair a stable project ID with its canonical root.
    #[must_use]
    pub const fn new(id: ProjectId, root: CanonicalRoot) -> Self {
        Self { id, root }
    }

    /// Return the stable project ID.
    #[must_use]
    pub const fn id(&self) -> &ProjectId {
        &self.id
    }

    /// Return the canonical project root.
    #[must_use]
    pub const fn root(&self) -> &CanonicalRoot {
        &self.root
    }
}

#[derive(Debug, Clone, Default)]
/// Resolver for canonical project roots.
pub struct ProjectResolver {
    projects: Vec<ProjectIdentity>,
}

impl ProjectResolver {
    /// Create a resolver after rejecting duplicate IDs and roots.
    ///
    /// # Errors
    ///
    /// Returns an error when IDs or canonical roots are duplicated.
    pub fn new(
        identities: impl IntoIterator<Item = ProjectIdentity>,
    ) -> Result<Self, ProjectIdentityError> {
        let mut ids = HashSet::new();
        let mut roots = HashSet::new();
        let mut projects = Vec::<ProjectIdentity>::new();

        for project in identities {
            if !ids.insert(project.id.clone()) {
                return Err(ProjectIdentityError::DuplicateId(project.id));
            }
            if !roots.insert(project.root.clone()) {
                return Err(ProjectIdentityError::DuplicateRoot(project.root.0));
            }
            projects.push(project);
        }

        Ok(Self { projects })
    }

    /// Resolve an existing path to the registered project with the longest root.
    ///
    /// # Errors
    ///
    /// Returns an error when the path cannot be canonicalized or no active project contains it.
    pub fn resolve_path(
        &self,
        path: impl AsRef<Path>,
    ) -> Result<&ProjectIdentity, ProjectIdentityError> {
        let path = path.as_ref();
        let canonical =
            path.canonicalize()
                .map_err(|source| ProjectIdentityError::Canonicalize {
                    path: path.to_path_buf(),
                    source,
                })?;

        self.projects
            .iter()
            .filter(|project| {
                project.root.as_path().exists() && canonical.starts_with(project.root.as_path())
            })
            .max_by_key(|project| project.root.as_path().components().count())
            .ok_or(ProjectIdentityError::UnregisteredPath(canonical))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    #[test]
    fn resolve_path_selects_longest_registered_root() {
        let workspace = TempDir::new().unwrap();
        let nested = workspace.path().join("nested");
        fs::create_dir(&nested).unwrap();
        let file = nested.join("src.rs");
        fs::write(&file, "fn main() {}").unwrap();

        let outer = ProjectIdentity::new(
            ProjectId::new("outer").unwrap(),
            CanonicalRoot::new(workspace.path()).unwrap(),
        );
        let inner = ProjectIdentity::new(
            ProjectId::new("inner").unwrap(),
            CanonicalRoot::new(&nested).unwrap(),
        );
        let project_resolver = ProjectResolver::new([outer, inner]).unwrap();

        let resolved = project_resolver.resolve_path(&file).unwrap();

        assert_eq!(resolved.id().as_str(), "inner");
    }

    #[test]
    fn new_rejects_duplicate_project_ids() {
        let first = TempDir::new().unwrap();
        let second = TempDir::new().unwrap();
        let projects = [
            ProjectIdentity::new(
                ProjectId::new("same").unwrap(),
                CanonicalRoot::new(first.path()).unwrap(),
            ),
            ProjectIdentity::new(
                ProjectId::new("same").unwrap(),
                CanonicalRoot::new(second.path()).unwrap(),
            ),
        ];

        assert!(matches!(
            ProjectResolver::new(projects),
            Err(ProjectIdentityError::DuplicateId(id)) if id.as_str() == "same"
        ));
    }

    #[cfg(unix)]
    #[test]
    fn resolve_path_canonicalizes_symlink_aliases() {
        use std::os::unix::fs::symlink;

        let workspace = TempDir::new().unwrap();
        let alias_parent = TempDir::new().unwrap();
        let alias = alias_parent.path().join("workspace");
        symlink(workspace.path(), &alias).unwrap();
        let file = workspace.path().join("src.rs");
        fs::write(&file, "fn main() {}").unwrap();

        let project = ProjectIdentity::new(
            ProjectId::new("workspace").unwrap(),
            CanonicalRoot::new(&alias).unwrap(),
        );
        let project_resolver = ProjectResolver::new([project]).unwrap();

        assert_eq!(
            project_resolver
                .resolve_path(alias.join("src.rs"))
                .unwrap()
                .id()
                .as_str(),
            "workspace"
        );
    }
}
