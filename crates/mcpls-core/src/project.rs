//! Project identity and canonical path routing primitives.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub(crate) enum ProjectIdentityError {
    #[error("project id must not be empty")]
    EmptyId,
    #[error("project root is not a directory: {path}")]
    RootNotDirectory { path: PathBuf },
    #[error("failed to canonicalize project path {path}: {source}")]
    Canonicalize {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("duplicate project id: {0}")]
    DuplicateId(ProjectId),
    #[error("duplicate project root: {0}")]
    DuplicateRoot(PathBuf),
    #[error("path is not registered to a project: {0}")]
    UnregisteredPath(PathBuf),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct ProjectId(String);

impl ProjectId {
    pub(crate) fn new(value: impl Into<String>) -> Result<Self, ProjectIdentityError> {
        let value = value.into();
        (!value.trim().is_empty())
            .then_some(Self(value))
            .ok_or(ProjectIdentityError::EmptyId)
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ProjectId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct CanonicalRoot(PathBuf);

impl CanonicalRoot {
    pub(crate) fn new(path: impl AsRef<Path>) -> Result<Self, ProjectIdentityError> {
        let path = path.as_ref();
        let canonical = path
            .canonicalize()
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

    pub(crate) fn as_path(&self) -> &Path {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProjectIdentity {
    id: ProjectId,
    root: CanonicalRoot,
}

impl ProjectIdentity {
    pub(crate) fn new(id: ProjectId, root: CanonicalRoot) -> Self {
        Self { id, root }
    }

    pub(crate) fn id(&self) -> &ProjectId {
        &self.id
    }

    pub(crate) fn root(&self) -> &CanonicalRoot {
        &self.root
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ProjectResolver {
    projects: Vec<ProjectIdentity>,
}

impl ProjectResolver {
    pub(crate) fn new(
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
                return Err(ProjectIdentityError::DuplicateRoot(
                    project.root.0.clone(),
                ));
            }
            projects.push(project);
        }

        Ok(Self { projects })
    }

    pub(crate) fn resolve_path(&self, path: impl AsRef<Path>) -> Result<&ProjectIdentity, ProjectIdentityError> {
        let path = path.as_ref();
        let canonical = path
            .canonicalize()
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
        let resolver = ProjectResolver::new([outer, inner]).unwrap();

        let resolved = resolver.resolve_path(&file).unwrap();

        assert_eq!(resolved.id().as_str(), "inner");
    }
}
