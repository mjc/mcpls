//! Project identity and canonical path routing.

#![allow(clippy::redundant_pub_crate)]

#[allow(clippy::wildcard_imports)]
use super::*;

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
    /// No project selector was supplied.
    #[error("a project ID or file path is required")]
    MissingSelector,
    /// The requested project ID is not registered.
    #[error("project is not registered: {0}")]
    ProjectNotFound(ProjectId),
    /// An explicit project ID does not contain the supplied path.
    #[error("path {path} does not belong to project {id}")]
    ProjectPathMismatch {
        /// The selected project ID.
        id: ProjectId,
        /// The mismatched path.
        path: PathBuf,
    },
    /// A registered project root no longer exists on disk.
    #[error("project root is unavailable: {0}")]
    ProjectRootUnavailable(ProjectId),
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
        let canonical = canonicalize(path)?;

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

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
/// Errors raised while resolving Git repository metadata.
pub enum GitRepositoryIdentityError {
    /// The supplied root cannot be canonicalized.
    #[error("failed to canonicalize Git root {path}: {source}")]
    Canonicalize {
        /// Root that could not be canonicalized.
        path: PathBuf,
        /// Underlying filesystem error.
        #[source]
        source: std::io::Error,
    },
    /// A `.git` file does not contain a valid `gitdir:` declaration.
    #[error("invalid Git metadata file: {path}")]
    InvalidGitFile {
        /// Metadata file path.
        path: PathBuf,
    },
    /// Git metadata points at a directory that no longer exists.
    #[error("Git metadata directory is unavailable: {path}")]
    MissingGitDirectory {
        /// Missing metadata directory.
        path: PathBuf,
    },
    /// A Git metadata file could not be read.
    #[error("failed to read Git metadata file {path}: {source}")]
    ReadMetadata {
        /// Metadata file path.
        path: PathBuf,
        /// Underlying filesystem error.
        #[source]
        source: std::io::Error,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
/// Canonical common Git directory shared by a checkout and linked worktrees.
pub struct GitRepositoryIdentity(PathBuf);

impl GitRepositoryIdentity {
    /// Resolve the common Git directory for a checkout, linked worktree, or bare repository.
    ///
    /// Returns `Ok(None)` for a non-Git directory.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed or stale Git metadata.
    pub fn discover(root: impl AsRef<Path>) -> Result<Option<Self>, GitRepositoryIdentityError> {
        let root = root.as_ref();
        let canonical_root =
            root.canonicalize()
                .map_err(|source| GitRepositoryIdentityError::Canonicalize {
                    path: root.to_path_buf(),
                    source,
                })?;
        let git_entry = canonical_root.join(".git");

        if git_entry.is_dir() {
            return Ok(Some(Self(git_entry.canonicalize().map_err(|source| {
                GitRepositoryIdentityError::Canonicalize {
                    path: git_entry.clone(),
                    source,
                }
            })?)));
        }

        if git_entry.is_file() {
            let metadata = std::fs::read_to_string(&git_entry).map_err(|source| {
                GitRepositoryIdentityError::ReadMetadata {
                    path: git_entry.clone(),
                    source,
                }
            })?;
            let target = metadata
                .lines()
                .find_map(|line| line.trim().strip_prefix("gitdir:"))
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| GitRepositoryIdentityError::InvalidGitFile {
                    path: git_entry.clone(),
                })?;
            let git_dir = {
                let target = PathBuf::from(target);
                if target.is_absolute() {
                    target
                } else {
                    canonical_root.join(target)
                }
            };
            let git_dir = canonicalize_git_metadata(git_dir)?;
            let common_dir = git_dir.join("commondir");
            if common_dir.is_file() {
                let relative = std::fs::read_to_string(&common_dir).map_err(|source| {
                    GitRepositoryIdentityError::ReadMetadata {
                        path: common_dir.clone(),
                        source,
                    }
                })?;
                let common = git_dir.join(relative.trim());
                return Ok(Some(Self(canonicalize_git_metadata(common)?)));
            }
            return Ok(Some(Self(git_dir)));
        }

        if canonical_root.join("HEAD").is_file()
            && canonical_root.join("config").is_file()
            && canonical_root.join("objects").is_dir()
        {
            return Ok(Some(Self(canonical_root)));
        }

        Ok(None)
    }

    /// Return the canonical common Git directory.
    #[must_use]
    pub fn common_dir(&self) -> &Path {
        &self.0
    }
}

pub(super) fn canonicalize_git_metadata(
    path: PathBuf,
) -> Result<PathBuf, GitRepositoryIdentityError> {
    path.canonicalize()
        .map_err(|_| GitRepositoryIdentityError::MissingGitDirectory { path })
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// Stable project ID paired with its canonical root.
pub struct ProjectIdentity {
    pub(super) id: ProjectId,
    pub(super) root: CanonicalRoot,
    pub(super) roots: Vec<CanonicalRoot>,
    pub(super) repository: Option<GitRepositoryIdentity>,
}

impl ProjectIdentity {
    /// Pair a stable project ID with its canonical root.
    #[must_use]
    pub fn new(id: ProjectId, root: CanonicalRoot) -> Self {
        Self {
            id,
            roots: vec![root.clone()],
            root,
            repository: None,
        }
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

    /// Return every canonical worktree root owned by this logical project.
    #[must_use]
    pub fn roots(&self) -> &[CanonicalRoot] {
        &self.roots
    }

    pub(crate) fn add_root(&mut self, root: CanonicalRoot) {
        if !self.roots.iter().any(|existing| existing == &root) {
            self.roots.push(root);
        }
    }

    /// Attach the shared Git repository identity for this checkout.
    #[must_use]
    pub fn with_repository_identity(mut self, repository: GitRepositoryIdentity) -> Self {
        self.repository = Some(repository);
        self
    }

    /// Return the shared Git repository identity, when this root is Git-backed.
    #[must_use]
    pub const fn repository_identity(&self) -> Option<&GitRepositoryIdentity> {
        self.repository.as_ref()
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
            for root in project.roots() {
                if !roots.insert(root.clone()) {
                    return Err(ProjectIdentityError::DuplicateRoot(root.0.clone()));
                }
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
        let canonical = match canonicalize(path) {
            Ok(canonical) => canonical,
            Err(error) => {
                if let Some(project) = self.projects.iter().find(|project| {
                    project
                        .roots()
                        .iter()
                        .any(|root| !root.as_path().exists() && path.starts_with(root.as_path()))
                }) {
                    return Err(ProjectIdentityError::ProjectRootUnavailable(
                        project.id.clone(),
                    ));
                }
                return Err(error);
            }
        };

        self.projects
            .iter()
            .filter_map(|project| {
                project
                    .roots()
                    .iter()
                    .filter(|root| root.as_path().exists() && canonical.starts_with(root.as_path()))
                    .max_by_key(|root| root.as_path().components().count())
                    .map(|root| (root.as_path().components().count(), project))
            })
            .max_by_key(|(components, _)| *components)
            .map(|(_, project)| project)
            .ok_or(ProjectIdentityError::UnregisteredPath(canonical))
    }

    /// Resolve by explicit project ID, optionally checking a file path.
    ///
    /// # Errors
    ///
    /// Returns an error when no selector is supplied, the ID is unknown, or the path
    /// is outside the selected project root.
    pub fn resolve(
        &self,
        project_id: Option<&ProjectId>,
        path: Option<&Path>,
    ) -> Result<&ProjectIdentity, ProjectIdentityError> {
        match (project_id, path) {
            (None, None) => Err(ProjectIdentityError::MissingSelector),
            (None, Some(path)) => self.resolve_path(path),
            (Some(project_id), None) => self.resolve_id(project_id),
            (Some(project_id), Some(path)) => {
                let project = self.resolve_id(project_id)?;
                let canonical = canonicalize(path)?;
                if project
                    .roots()
                    .iter()
                    .any(|root| root.as_path().exists() && canonical.starts_with(root.as_path()))
                {
                    Ok(project)
                } else {
                    Err(ProjectIdentityError::ProjectPathMismatch {
                        id: project_id.clone(),
                        path: canonical,
                    })
                }
            }
        }
    }

    /// Resolve an explicit project ID.
    ///
    /// # Errors
    ///
    /// Returns [`ProjectIdentityError::ProjectNotFound`] when the ID is not registered.
    pub fn resolve_id(
        &self,
        project_id: &ProjectId,
    ) -> Result<&ProjectIdentity, ProjectIdentityError> {
        self.projects
            .iter()
            .find(|project| project.id() == project_id)
            .ok_or_else(|| ProjectIdentityError::ProjectNotFound(project_id.clone()))
    }
}

/// Return the registered root with the most path components that contains `path`.
#[must_use]
pub fn longest_matching_root<'a>(path: &Path, roots: &'a [PathBuf]) -> Option<&'a Path> {
    roots
        .iter()
        .filter(|root| path.starts_with(root))
        .max_by_key(|root| root.components().count())
        .map(PathBuf::as_path)
}

pub(super) fn canonicalize(path: &Path) -> Result<PathBuf, ProjectIdentityError> {
    path.canonicalize()
        .map_err(|source| ProjectIdentityError::Canonicalize {
            path: path.to_path_buf(),
            source,
        })
}

pub(super) fn resolve_edit_safety_path(boundary: &WorkspaceBoundary, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        boundary.root().join(path)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ProjectCompatibilityKey([u8; 32]);

pub(super) fn has_dynamic_project_environment(root: &Path) -> bool {
    [".envrc", "flake.nix"]
        .into_iter()
        .any(|marker| root.join(marker).is_file())
}

/// Return a conservative fingerprint for the inputs that shape Rust analysis.
///
/// A missing explicit toolchain or Cargo manifest is deliberately treated as
/// unknown rather than compatible. Manifest and lockfile contents are not
/// process-wide constraints: rust-analyzer receives each manifest separately
/// through `linkedProjects`.
pub(super) async fn rust_project_compatibility_key(
    root: &Path,
    translator_template: Option<&TranslatorTemplate>,
) -> Option<ProjectCompatibilityKey> {
    const INPUTS: &[&str] = &[
        "rust-toolchain",
        "rust-toolchain.toml",
        ".cargo/config",
        ".cargo/config.toml",
    ];

    let project_environment =
        if translator_template.is_some() || has_dynamic_project_environment(root) {
            load_project_environment(root).await
        } else {
            None
        };
    if has_dynamic_project_environment(root) && project_environment.is_none() {
        return None;
    }

    let mut hasher = Sha256::new();
    let mut has_toolchain = false;
    for relative in INPUTS {
        let path = root.join(relative);
        match std::fs::read(&path) {
            Ok(contents) => {
                has_toolchain |=
                    *relative == "rust-toolchain" || *relative == "rust-toolchain.toml";
                hash_compatibility_field(&mut hasher, relative.as_bytes());
                hash_compatibility_field(&mut hasher, &contents);
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {
                hash_compatibility_field(&mut hasher, relative.as_bytes());
                hash_compatibility_field(&mut hasher, &[]);
            }
            Err(_) => return None,
        }
    }

    if let Some(template) = translator_template {
        let toolchain_signature = rust_toolchain_signature(root)?;
        hash_compatibility_field(&mut hasher, b"rustc-toolchain-v1");
        hash_compatibility_field(&mut hasher, &toolchain_signature);
        hash_rust_server_config(&mut hasher, template, project_environment.as_ref())?;
    }

    (has_toolchain && root.join("Cargo.toml").is_file())
        .then(|| ProjectCompatibilityKey(hasher.finalize().into()))
}

pub(super) fn hash_rust_server_config(
    hasher: &mut Sha256,
    template: &TranslatorTemplate,
    project_environment: Option<&std::collections::HashMap<String, Option<String>>>,
) -> Option<()> {
    let config = template.rust_server_config()?;
    hasher.update(b"rust-server-config-v1");
    hash_compatibility_field(hasher, config.language_id.as_bytes());
    hash_compatibility_field(hasher, config.command.as_bytes());
    let resolved_command = resolve_command(&config.command, project_environment);
    let resolved_command = resolved_command.canonicalize().ok()?;
    hash_compatibility_field(hasher, b"resolved-command-v1");
    hash_compatibility_field(hasher, resolved_command.to_string_lossy().as_bytes());
    hash_compatibility_strings(hasher, &config.file_patterns);
    hash_compatibility_strings(hasher, &config.args);
    let mut environment = config.env.iter().collect::<Vec<_>>();
    environment.sort_unstable_by(|left, right| left.0.cmp(right.0));
    for (name, value) in environment {
        hash_compatibility_field(hasher, name.as_bytes());
        hash_compatibility_field(hasher, value.as_bytes());
    }
    hash_project_environment(hasher, project_environment);
    let initialization_options = serde_json::to_vec(&config.initialization_options).ok()?;
    hash_compatibility_field(hasher, &initialization_options);
    if let Some(edit_safety) = template.edit_safety() {
        let edit_safety = serde_json::to_vec(edit_safety).ok()?;
        hash_compatibility_field(hasher, b"edit-safety-policy-v1");
        hash_compatibility_field(hasher, &edit_safety);
    }
    hash_compatibility_field(hasher, &config.timeout_seconds.to_le_bytes());
    if let Some(heuristics) = &config.heuristics {
        hash_compatibility_strings(hasher, &heuristics.project_markers);
    }
    hash_compatibility_field(
        hasher,
        &template
            .heuristics_max_depth()
            .unwrap_or_default()
            .to_le_bytes(),
    );
    Some(())
}

pub(super) fn hash_project_environment(
    hasher: &mut Sha256,
    project_environment: Option<&std::collections::HashMap<String, Option<String>>>,
) {
    let Some(project_environment) = project_environment else {
        return;
    };
    let mut entries = project_environment
        .iter()
        .filter(|(name, _)| !is_ephemeral_environment_key(name))
        .collect::<Vec<_>>();
    entries.sort_unstable_by(|left, right| left.0.cmp(right.0));
    for (name, value) in entries {
        hash_compatibility_field(hasher, name.as_bytes());
        match value {
            Some(value) => {
                hash_compatibility_field(hasher, b"set");
                hash_compatibility_field(hasher, value.as_bytes());
            }
            None => hash_compatibility_field(hasher, b"unset"),
        }
    }
}

pub(super) fn is_ephemeral_environment_key(name: &str) -> bool {
    matches!(name, "PWD" | "OLDPWD" | "SHLVL" | "_") || name.starts_with("DIRENV_")
}

pub(super) fn hash_compatibility_strings(hasher: &mut Sha256, values: &[String]) {
    for value in values {
        hash_compatibility_field(hasher, value.as_bytes());
    }
}

pub(super) fn rust_toolchain_signature(root: &Path) -> Option<Vec<u8>> {
    let channel = rust_toolchain_channel(root)?;
    probe_rustc_version(&channel)
}

pub(super) fn probe_rustc_version(channel: &str) -> Option<Vec<u8>> {
    let mut rustup = Command::new("rustup");
    rustup.args(["run", channel, "rustc", "-Vv"]);
    match rustup.output() {
        Ok(output) if output.status.success() => Some(output.stdout),
        Err(error) if error.kind() == ErrorKind::NotFound => probe_direct_rustc(channel),
        Ok(_) | Err(_) => None,
    }
}

pub(super) fn probe_direct_rustc(channel: &str) -> Option<Vec<u8>> {
    Command::new("rustc")
        .arg(format!("+{channel}"))
        .arg("-Vv")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| output.stdout)
}

pub(super) fn rust_toolchain_channel(root: &Path) -> Option<String> {
    ["rust-toolchain", "rust-toolchain.toml"]
        .into_iter()
        .find_map(|relative| {
            let contents = std::fs::read_to_string(root.join(relative)).ok()?;
            parse_rust_toolchain_channel(relative, &contents)
        })
}

pub(super) fn parse_rust_toolchain_channel(relative: &str, contents: &str) -> Option<String> {
    if relative == "rust-toolchain.toml" {
        contents.lines().find_map(|line| {
            let (key, value) = line.split_once('=')?;
            (key.trim() == "channel").then(|| value.trim().trim_matches(['"', '\'']).to_owned())
        })
    } else {
        contents
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .map(str::to_owned)
    }
}

pub(super) fn hash_compatibility_field(hasher: &mut Sha256, field: &[u8]) {
    hasher.update((field.len() as u64).to_le_bytes());
    hasher.update(field);
}

#[cfg(test)]
mod tests {
    use super::{CanonicalRoot, ProjectId, ProjectIdentity};

    #[test]
    fn identity_deduplicates_shared_worktree_roots() -> Result<(), Box<dyn std::error::Error>> {
        let first = tempfile::tempdir()?;
        let second = tempfile::tempdir()?;
        let id = ProjectId::new("shared")?;
        let first_root = CanonicalRoot::new(first.path())?;
        let second_root = CanonicalRoot::new(second.path())?;
        let mut identity = ProjectIdentity::new(id, first_root);

        identity.add_root(second_root.clone());
        identity.add_root(second_root);

        assert_eq!(identity.roots().len(), 2);
        Ok(())
    }
}
