//! Project identity and canonical path routing primitives.

mod residency;

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex, Notify, RwLock, broadcast, mpsc, oneshot, watch};

use crate::bridge::convert_code_action_or_command;
use crate::bridge::{
    ActivationHealth, CallHierarchyPrepareResult, CodeActionsResult, CompletionsResult,
    DefinitionResult, DiagnosticSeverity, DiagnosticsResult, DocumentSymbolsResult,
    FormatDocumentResult, HoverResult, IncomingCallsResult, InlayHintsResult, LocationsResult,
    LogEntry, LogLevel, OutgoingCallsResult, PositionEncoding, ProjectActivation,
    ProviderSynchronization, ReferencesResult, RenameResult, SemanticDiscoveryKind,
    SemanticDiscoveryResult, ServerCapability, ServerLogsResult, ServerMessage,
    ServerMessagesResult, SignatureHelpResult, StructuralMatch, StructuralSearchResult,
    SupportedWorkspaceEdit, Translator, TranslatorTemplate, WillRenameFilesResult,
    WorkspaceSymbolResult, path_to_uri, uri_to_path,
};
use crate::config::{EditSafetyConfig, ProjectConfig, ServerId};
use crate::edit_apply::{
    ApplyReport, apply_plan_with_documents, apply_plan_with_documents_and_backup,
};
use crate::edit_backup::BackupPolicy;
use crate::edit_paths::WorkspaceBoundary;
use crate::edit_plan::{AuditLogPolicy, EditAuditRecord, EditPlan, EditPlanStore, PlanId};
use crate::edit_preview::{
    EditProducer, PreviewArtifact, PreviewLimits, VerificationStatus, preview_workspace_edit,
};
use crate::lsp::{LspNotification, load_project_environment, resolve_command};
use crate::project_persistence::{PersistedProject, ProjectRegistrationStore};
use crate::rust_refactor::{logical_module_name, move_inline_module_preview_with_source};
use crate::workspace_edit::{EditOperation, normalize};
use lsp_types::WorkspaceEdit;
use residency::{RustGroupId, RustResidencyController};

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

fn canonicalize_git_metadata(path: PathBuf) -> Result<PathBuf, GitRepositoryIdentityError> {
    path.canonicalize()
        .map_err(|_| GitRepositoryIdentityError::MissingGitDirectory { path })
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// Stable project ID paired with its canonical root.
pub struct ProjectIdentity {
    id: ProjectId,
    root: CanonicalRoot,
    roots: Vec<CanonicalRoot>,
    repository: Option<GitRepositoryIdentity>,
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

fn canonicalize(path: &Path) -> Result<PathBuf, ProjectIdentityError> {
    path.canonicalize()
        .map_err(|source| ProjectIdentityError::Canonicalize {
            path: path.to_path_buf(),
            source,
        })
}

fn resolve_edit_safety_path(boundary: &WorkspaceBoundary, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        boundary.root().join(path)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ProjectCompatibilityKey([u8; 32]);

fn has_dynamic_project_environment(root: &Path) -> bool {
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
async fn rust_project_compatibility_key(
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

fn hash_rust_server_config(
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

fn hash_project_environment(
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

fn is_ephemeral_environment_key(name: &str) -> bool {
    matches!(name, "PWD" | "OLDPWD" | "SHLVL" | "_") || name.starts_with("DIRENV_")
}

fn hash_compatibility_strings(hasher: &mut Sha256, values: &[String]) {
    for value in values {
        hash_compatibility_field(hasher, value.as_bytes());
    }
}

fn rust_toolchain_signature(root: &Path) -> Option<Vec<u8>> {
    let channel = rust_toolchain_channel(root)?;
    probe_rustc_version(&channel)
}

fn probe_rustc_version(channel: &str) -> Option<Vec<u8>> {
    let mut rustup = Command::new("rustup");
    rustup.args(["run", channel, "rustc", "-Vv"]);
    match rustup.output() {
        Ok(output) if output.status.success() => Some(output.stdout),
        Err(error) if error.kind() == ErrorKind::NotFound => probe_direct_rustc(channel),
        Ok(_) | Err(_) => None,
    }
}

fn probe_direct_rustc(channel: &str) -> Option<Vec<u8>> {
    Command::new("rustc")
        .arg(format!("+{channel}"))
        .arg("-Vv")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| output.stdout)
}

fn rust_toolchain_channel(root: &Path) -> Option<String> {
    ["rust-toolchain", "rust-toolchain.toml"]
        .into_iter()
        .find_map(|relative| {
            let contents = std::fs::read_to_string(root.join(relative)).ok()?;
            parse_rust_toolchain_channel(relative, &contents)
        })
}

fn parse_rust_toolchain_channel(relative: &str, contents: &str) -> Option<String> {
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

fn hash_compatibility_field(hasher: &mut Sha256, field: &[u8]) {
    hasher.update((field.len() as u64).to_le_bytes());
    hasher.update(field);
}

/// Observable lifecycle state for one project actor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ProjectStatus {
    /// The actor exists but its language servers are not ready.
    Starting,
    /// The project can accept requests.
    Ready,
    /// The project is available with at least one degraded component.
    Degraded,
    /// The project is replacing or restarting a language server.
    Restarting,
    /// The actor remains registered but owns no resident language server.
    Dormant,
    /// The actor is draining work before shutdown.
    Stopping,
    /// The actor has stopped and accepts no new requests.
    Stopped,
    /// The project failed and requires recovery or explicit restart.
    Failed,
}

impl ProjectStatus {
    /// Return the stable wire spelling for this lifecycle state.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "Starting",
            Self::Ready => "Ready",
            Self::Degraded => "Degraded",
            Self::Restarting => "Restarting",
            Self::Dormant => "Dormant",
            Self::Stopping => "Stopping",
            Self::Stopped => "Stopped",
            Self::Failed => "Failed",
        }
    }
}

/// Typed events emitted by a project actor for session-facing delivery.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ProjectEvent {
    /// The actor's lifecycle status changed.
    StatusChanged {
        /// New lifecycle status.
        status: ProjectStatus,
        /// Failure detail associated with the new status, if any.
        last_error: Option<String>,
    },
    /// The current language-server notification stream ended unexpectedly.
    ServerExited {
        /// Runtime generation that exited.
        generation: u64,
    },
    /// A language server published diagnostics for one document.
    DiagnosticsUpdated {
        /// Document URI whose diagnostics were replaced.
        uri: String,
        /// LSP document version, when provided by the server.
        version: Option<i32>,
        /// Number of diagnostics in the replacement set.
        diagnostic_count: usize,
    },
    /// Files changed by a completed workspace edit.
    FilesChanged {
        /// Files written, created, renamed, or deleted by the edit.
        paths: Vec<PathBuf>,
    },
    /// A workspace edit plan completed successfully.
    EditApplied {
        /// Opaque identifier of the consumed edit plan.
        plan_id: PlanId,
        /// Files changed by the completed edit.
        committed_files: Vec<PathBuf>,
        /// Number of text and resource operations in the plan.
        operation_count: usize,
    },
    /// A registered project identity was removed from the shared registry.
    ProjectRemoved {
        /// Stable identity that is no longer routable.
        project_id: ProjectId,
        /// Canonical worktree root whose file resources are no longer valid.
        root: PathBuf,
    },
}

impl ProjectEvent {
    /// Return whether this event belongs to the project receiving it.
    #[must_use]
    pub(crate) fn belongs_to(&self, project_id: &ProjectId) -> bool {
        !matches!(
            self,
            Self::ProjectRemoved {
                project_id: removed_project,
                ..
            } if removed_project != project_id
        )
    }

    /// Encode the stable wire representation used by project-event resources.
    #[must_use]
    pub fn json_value(&self) -> serde_json::Value {
        match self {
            Self::StatusChanged { status, last_error } => serde_json::json!({
                "kind": "status_changed",
                "status": format!("{status:?}"),
                "last_error": last_error,
            }),
            Self::ServerExited { generation } => serde_json::json!({
                "kind": "server_exited",
                "generation": generation,
            }),
            Self::DiagnosticsUpdated {
                uri,
                version,
                diagnostic_count,
            } => serde_json::json!({
                "kind": "diagnostics_updated",
                "uri": uri,
                "version": version,
                "diagnostic_count": diagnostic_count,
            }),
            Self::FilesChanged { paths } => serde_json::json!({
                "kind": "files_changed",
                "paths": paths,
            }),
            Self::EditApplied {
                plan_id,
                committed_files,
                operation_count,
            } => serde_json::json!({
                "kind": "edit_applied",
                "plan_id": plan_id.as_str(),
                "committed_files": committed_files,
                "operation_count": operation_count,
            }),
            Self::ProjectRemoved { project_id, root } => serde_json::json!({
                "kind": "project_removed",
                "project_id": project_id.as_str(),
                "root": root,
            }),
        }
    }
}

/// One ordered project event retained for cursor-based session polling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectEventRecord {
    sequence: u64,
    event: ProjectEvent,
}

impl ProjectEventRecord {
    /// Return the monotonically increasing event sequence.
    #[must_use]
    pub const fn sequence(&self) -> u64 {
        self.sequence
    }

    /// Return the typed event payload.
    #[must_use]
    pub const fn event(&self) -> &ProjectEvent {
        &self.event
    }

    /// Encode this ordered event record for resource polling clients.
    #[must_use]
    pub fn json_value(&self) -> serde_json::Value {
        serde_json::json!({
            "sequence": self.sequence,
            "event": self.event.json_value(),
        })
    }
}

/// Bounded event history snapshot returned to session polling clients.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectEventSnapshot {
    events: Vec<ProjectEventRecord>,
    resync_required: bool,
    next_sequence: u64,
}

impl ProjectEventSnapshot {
    /// Return events newer than the requested cursor.
    #[must_use]
    pub fn events(&self) -> &[ProjectEventRecord] {
        &self.events
    }

    /// Whether the requested cursor predates the retained bounded history.
    #[must_use]
    pub const fn resync_required(&self) -> bool {
        self.resync_required
    }

    /// Return the next cursor clients should use for a subsequent poll.
    #[must_use]
    pub const fn next_sequence(&self) -> u64 {
        self.next_sequence
    }
}

/// Bounded actor-owned project event history.
#[derive(Debug)]
pub struct ProjectEventHistory {
    records: VecDeque<ProjectEventRecord>,
    capacity: usize,
    next_sequence: u64,
}

impl ProjectEventHistory {
    /// Create a bounded history with at least one retained event.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            records: VecDeque::with_capacity(capacity.max(1)),
            capacity: capacity.max(1),
            next_sequence: 1,
        }
    }

    /// Record one event and return its assigned sequence.
    pub fn record(&mut self, event: ProjectEvent) -> u64 {
        let sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.saturating_add(1);
        if self.records.len() == self.capacity {
            self.records.pop_front();
        }
        self.records
            .push_back(ProjectEventRecord { sequence, event });
        sequence
    }

    /// Return retained events newer than `cursor`, marking overflow when needed.
    #[must_use]
    pub fn snapshot_since(&self, cursor: Option<u64>) -> ProjectEventSnapshot {
        let oldest = self
            .records
            .front()
            .map_or(self.next_sequence, |record| record.sequence);
        let resync_required = cursor.is_some_and(|cursor| cursor < oldest.saturating_sub(1));
        let events = self
            .records
            .iter()
            .filter(|record| cursor.is_none_or(|cursor| record.sequence > cursor))
            .cloned()
            .collect();
        ProjectEventSnapshot {
            events,
            resync_required,
            next_sequence: self.next_sequence,
        }
    }
}

/// Observable project state, including the most recent failure detail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectState {
    status: ProjectStatus,
    last_error: Option<String>,
    runtime: ProjectRuntimeSummary,
}

impl ProjectState {
    const fn new(status: ProjectStatus, runtime: ProjectRuntimeSummary) -> Self {
        Self {
            status,
            last_error: None,
            runtime,
        }
    }

    /// Return the current lifecycle status.
    #[must_use]
    pub const fn status(&self) -> ProjectStatus {
        self.status
    }

    /// Return the most recent actor failure, if one was recorded.
    #[must_use]
    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    /// Return the project-local runtime summary owned by the actor.
    #[must_use]
    pub const fn runtime(&self) -> &ProjectRuntimeSummary {
        &self.runtime
    }

    /// Return the canonical workspace roots owned by this project actor.
    #[must_use]
    pub fn workspace_roots(&self) -> &[PathBuf] {
        self.runtime.workspace_roots()
    }

    /// Return the number of open documents owned by this project actor.
    #[must_use]
    pub const fn open_document_count(&self) -> usize {
        self.runtime.open_document_count()
    }

    fn sync_runtime(&mut self, runtime: &ProjectRuntime) {
        self.runtime = runtime.summary();
    }

    fn aggregate(states: impl IntoIterator<Item = Self>) -> Self {
        let mut states = states.into_iter();
        let Some(mut aggregate) = states.next() else {
            return Self::new(ProjectStatus::Starting, ProjectRuntimeSummary::default());
        };
        for state in states {
            aggregate.merge(state);
        }
        aggregate
    }

    fn merge(&mut self, state: Self) {
        let state_priority = project_status_priority(state.status);
        if state_priority >= project_status_priority(self.status) {
            self.status = state.status;
            self.last_error = state.last_error;
        }
        self.runtime.merge(state.runtime);
    }
}

const fn project_status_priority(status: ProjectStatus) -> u8 {
    match status {
        ProjectStatus::Failed => 6,
        ProjectStatus::Stopping => 5,
        ProjectStatus::Restarting => 4,
        ProjectStatus::Degraded => 3,
        ProjectStatus::Starting => 2,
        ProjectStatus::Ready => 1,
        ProjectStatus::Dormant | ProjectStatus::Stopped => 0,
    }
}

/// Project-local state counts and roots owned by an actor.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProjectRuntimeSummary {
    workspace_roots: Vec<PathBuf>,
    configured_language_ids: Vec<String>,
    active_language_ids: Vec<String>,
    open_document_count: usize,
    generation: u64,
}

impl ProjectRuntimeSummary {
    fn from_translator(translator: &Translator, generation: u64) -> Self {
        Self {
            workspace_roots: translator.workspace_roots().to_vec(),
            configured_language_ids: translator.configured_language_ids(),
            active_language_ids: translator.active_language_ids(),
            open_document_count: translator.open_document_count(),
            generation,
        }
    }

    /// Return the workspace roots owned by the actor.
    #[must_use]
    pub fn workspace_roots(&self) -> &[PathBuf] {
        &self.workspace_roots
    }

    /// Return configured language IDs.
    #[must_use]
    pub fn configured_language_ids(&self) -> &[String] {
        &self.configured_language_ids
    }

    /// Return active language IDs.
    #[must_use]
    pub fn active_language_ids(&self) -> &[String] {
        &self.active_language_ids
    }

    /// Return the number of open documents.
    #[must_use]
    pub const fn open_document_count(&self) -> usize {
        self.open_document_count
    }

    /// Return the actor's current LSP lifecycle generation.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    fn merge(&mut self, other: Self) {
        self.workspace_roots.extend(other.workspace_roots);
        self.configured_language_ids
            .extend(other.configured_language_ids);
        self.active_language_ids.extend(other.active_language_ids);
        self.open_document_count += other.open_document_count;
        self.generation = self.generation.max(other.generation);
        self.workspace_roots.sort();
        self.workspace_roots.dedup();
        self.configured_language_ids.sort();
        self.configured_language_ids.dedup();
        self.active_language_ids.sort();
        self.active_language_ids.dedup();
    }
}

/// Errors returned when a project actor cannot service a request.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ProjectActorError {
    /// The actor request channel has closed.
    #[error("project actor is closed")]
    Closed,
    /// The actor dropped a response before replying.
    #[error("project actor cancelled the request")]
    Cancelled,
    /// The actor operation failed after it started.
    #[error("project actor operation failed: {0}")]
    Operation(String),
}

#[derive(Clone)]
struct ProjectRequestGate {
    accepting: std::sync::Arc<AtomicBool>,
    rejected: std::sync::Arc<Notify>,
}

impl ProjectRequestGate {
    fn new() -> Self {
        Self {
            accepting: std::sync::Arc::new(AtomicBool::new(true)),
            rejected: std::sync::Arc::new(Notify::new()),
        }
    }

    fn reject_new_work(&self) {
        self.accepting.store(false, Ordering::Release);
        self.rejected.notify_waiters();
    }

    fn accept_new_work(&self) {
        self.accepting.store(true, Ordering::Release);
    }

    fn is_accepting(&self) -> bool {
        self.accepting.load(Ordering::Acquire)
    }

    async fn wait_for_rejection(&self) {
        self.rejected.notified().await;
    }
}

#[derive(Clone)]
struct ProjectRequestSender {
    sender: mpsc::Sender<ProjectRequest>,
    gate: ProjectRequestGate,
    residency: Option<ProjectResidency>,
}

impl ProjectRequestSender {
    fn new(sender: mpsc::Sender<ProjectRequest>) -> Self {
        Self {
            sender,
            gate: ProjectRequestGate::new(),
            residency: None,
        }
    }

    fn with_residency(sender: mpsc::Sender<ProjectRequest>, residency: ProjectResidency) -> Self {
        Self {
            sender,
            gate: ProjectRequestGate::new(),
            residency: Some(residency),
        }
    }

    fn queue_pressure(&self) -> ProjectQueuePressure {
        ProjectQueuePressure {
            queued: self.sender.max_capacity() - self.sender.capacity(),
            capacity: self.sender.max_capacity(),
        }
    }

    fn same_channel(&self, other: &Self) -> bool {
        self.sender.same_channel(&other.sender)
    }

    fn reject_new_work(&self) {
        self.gate.reject_new_work();
    }

    fn accept_new_work(&self) {
        self.gate.accept_new_work();
    }

    async fn send(
        &self,
        mut request: ProjectRequest,
    ) -> Result<(), mpsc::error::SendError<ProjectRequest>> {
        if !self.gate.is_accepting() {
            return Err(mpsc::error::SendError(request));
        }

        if request.uses_rust_residency()
            && let Some(residency) = &self.residency
        {
            request = residency.resident_request(request).await;
        }

        let permit = tokio::select! {
            result = self.sender.clone().reserve_owned() => match result {
                Ok(permit) => permit,
                Err(_) => return Err(mpsc::error::SendError(request)),
            },
            () = self.gate.wait_for_rejection() => {
                return Err(mpsc::error::SendError(request));
            }
        };
        if !self.gate.is_accepting() {
            return Err(mpsc::error::SendError(request));
        }
        permit.send(request);
        Ok(())
    }

    // Lifecycle control must still reach the actor after normal work is rejected.
    async fn send_unchecked(
        &self,
        request: ProjectRequest,
    ) -> Result<(), mpsc::error::SendError<ProjectRequest>> {
        self.sender.send(request).await
    }
}

/// Result of consuming and applying one project-owned edit plan.
#[derive(Debug, PartialEq, Eq)]
pub struct AppliedEditPlan {
    /// Opaque identifier of the consumed plan.
    pub plan_id: PlanId,
    /// Human-readable operations captured by the preview.
    pub operations: Vec<String>,
    /// Unified diff captured by the preview.
    pub unified_diff: String,
    /// Files replaced successfully.
    pub committed_files: Vec<PathBuf>,
    /// Optional semantic verification outcome for a specialized refactor.
    pub verification: Option<VerificationStatus>,
    /// Post-commit provider convergence results for workspace changes.
    pub provider_synchronization: Vec<ProviderSynchronization>,
}

impl AppliedEditPlan {
    fn project_events(&self) -> [ProjectEvent; 2] {
        [
            ProjectEvent::FilesChanged {
                paths: self.committed_files.clone(),
            },
            ProjectEvent::EditApplied {
                plan_id: self.plan_id.clone(),
                committed_files: self.committed_files.clone(),
                operation_count: self.operations.len(),
            },
        ]
    }
}

fn merge_provider_synchronization(
    results: &mut Vec<ProviderSynchronization>,
    result: ProviderSynchronization,
) {
    let Some(existing) = results
        .iter_mut()
        .find(|existing| existing.provider == result.provider)
    else {
        results.push(result);
        return;
    };
    existing.synchronized &= result.synchronized;
    existing.watched_file_notifications = existing
        .watched_file_notifications
        .saturating_add(result.watched_file_notifications);
    if let Some(message) = result.message {
        existing.message = Some(existing.message.take().map_or_else(
            || message.clone(),
            |current| format!("{current}; {message}"),
        ));
    }
}

fn planned_text_changes(plan: &EditPlan) -> Vec<(PathBuf, String)> {
    plan.files()
        .iter()
        .filter(|snapshot| snapshot.original_content() != snapshot.planned_content())
        .map(|snapshot| {
            (
                snapshot.path().clone(),
                snapshot.planned_content().to_string(),
            )
        })
        .collect()
}

/// Explicit syntax accepted by the structural preview tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StructuralDialect {
    /// rust-analyzer's `experimental/ssr` rule syntax.
    RustAnalyzerSsr,
    /// ast-grep's pattern and replacement-template syntax.
    AstGrep,
}

impl StructuralDialect {
    /// Return the stable MCP wire value.
    #[must_use]
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::RustAnalyzerSsr => "rust_analyzer_ssr",
            Self::AstGrep => "ast_grep",
        }
    }

    /// Return the implementation selected by this explicit dialect.
    #[must_use]
    pub(crate) const fn engine(self) -> &'static str {
        match self {
            Self::RustAnalyzerSsr => "rust_analyzer",
            Self::AstGrep => "ast_grep",
        }
    }
}

/// Actor-owned inputs for one structural search or replacement preview.
#[derive(Debug, Clone)]
pub(crate) struct StructuralReplaceRequest {
    pub(crate) file_path: String,
    pub(crate) dialect: StructuralDialect,
    pub(crate) query: String,
    pub(crate) replacement: Option<String>,
    pub(crate) language_id: Option<String>,
    pub(crate) parse_only: bool,
    pub(crate) encoding: PositionEncoding,
}

/// Write-free result of a structural search or replacement request.
#[derive(Debug, Clone)]
pub(crate) struct StructuralPreview {
    /// Stored plan when a replacement matched and was previewed.
    pub(crate) artifact: Option<PreviewArtifact>,
    /// Explicit parser/replacement syntax selected by the caller.
    pub(crate) dialect: StructuralDialect,
    /// Matched source ranges before replacement.
    pub(crate) matches: Vec<StructuralMatch>,
    /// Whether only parser validation was requested.
    pub(crate) parse_only: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct PathRenameRequest {
    pub(crate) old_path: String,
    pub(crate) new_path: String,
    pub(crate) encoding: PositionEncoding,
}

#[derive(Debug, Clone)]
pub(crate) struct PathRenamePreview {
    pub(crate) artifact: PreviewArtifact,
    pub(crate) providers: Vec<String>,
    pub(crate) semantic_edit_count: usize,
}

enum ProjectRequest {
    Resident {
        request: Box<Self>,
        guard: residency::RustResidencyGuard,
    },
    Query {
        reply: oneshot::Sender<ProjectState>,
    },
    SetStatus {
        status: ProjectStatus,
        reply: oneshot::Sender<()>,
    },
    Refresh {
        reply: oneshot::Sender<ProjectState>,
    },
    Activate {
        root: PathBuf,
        reply: oneshot::Sender<Result<ProjectState, String>>,
    },
    ActivateWorkspaceRoots {
        roots: Vec<PathBuf>,
        reply: oneshot::Sender<Result<ProjectState, String>>,
    },
    Hover {
        file_path: String,
        line: u32,
        character: u32,
        reply: oneshot::Sender<Result<HoverResult, String>>,
    },
    Definition {
        file_path: String,
        line: u32,
        character: u32,
        reply: oneshot::Sender<Result<DefinitionResult, String>>,
    },
    References {
        file_path: String,
        line: u32,
        character: u32,
        include_declaration: bool,
        reply: oneshot::Sender<Result<ReferencesResult, String>>,
    },
    Diagnostics {
        file_path: String,
        reply: oneshot::Sender<Result<DiagnosticsResult, String>>,
    },
    Rename {
        file_path: String,
        line: u32,
        character: u32,
        new_name: String,
        reply: oneshot::Sender<Result<RenameResult, String>>,
    },
    RenameWorkspaceEdit {
        file_path: String,
        line: u32,
        character: u32,
        new_name: String,
        reply: oneshot::Sender<Result<Option<WorkspaceEdit>, String>>,
    },
    Completions {
        file_path: String,
        line: u32,
        character: u32,
        trigger: Option<String>,
        reply: oneshot::Sender<Result<CompletionsResult, String>>,
    },
    DocumentSymbols {
        file_path: String,
        reply: oneshot::Sender<Result<DocumentSymbolsResult, String>>,
    },
    FormatDocument {
        file_path: String,
        tab_size: u32,
        insert_spaces: bool,
        reply: oneshot::Sender<Result<FormatDocumentResult, String>>,
    },
    FormatWorkspaceEdit {
        file_path: String,
        tab_size: u32,
        insert_spaces: bool,
        reply: oneshot::Sender<Result<Option<WorkspaceEdit>, String>>,
    },
    RangeFormatWorkspaceEdit {
        file_path: String,
        start: (u32, u32),
        end: (u32, u32),
        tab_size: u32,
        insert_spaces: bool,
        reply: oneshot::Sender<Result<SupportedWorkspaceEdit, String>>,
    },
    MoveItemWorkspaceEdit {
        file_path: String,
        start: (u32, u32),
        end: (u32, u32),
        direction: String,
        reply: oneshot::Sender<Result<SupportedWorkspaceEdit, String>>,
    },
    SemanticDiscovery {
        file_path: String,
        line: u32,
        character: u32,
        kind: SemanticDiscoveryKind,
        reply: oneshot::Sender<Result<SemanticDiscoveryResult, String>>,
    },
    WorkspaceSymbol {
        query: String,
        kind_filter: Option<String>,
        limit: u32,
        reply: oneshot::Sender<Result<WorkspaceSymbolResult, String>>,
    },
    CodeActions {
        file_path: String,
        start_line: u32,
        start_character: u32,
        end_line: u32,
        end_character: u32,
        kind_filter: Option<String>,
        reply: oneshot::Sender<Result<CodeActionsResult, String>>,
    },
    CodeActionList {
        file_path: String,
        start_line: u32,
        start_character: u32,
        end_line: u32,
        end_character: u32,
        kind_filter: Option<String>,
        reply: oneshot::Sender<Result<CodeActionsResult, String>>,
    },
    CodeActionPreview {
        action_id: PlanId,
        project_id: String,
        encoding: PositionEncoding,
        root: PathBuf,
        reply: oneshot::Sender<Result<PreviewArtifact, String>>,
    },
    PrepareCallHierarchy {
        file_path: String,
        line: u32,
        character: u32,
        reply: oneshot::Sender<Result<CallHierarchyPrepareResult, String>>,
    },
    IncomingCalls {
        item: serde_json::Value,
        reply: oneshot::Sender<Result<IncomingCallsResult, String>>,
    },
    OutgoingCalls {
        item: serde_json::Value,
        reply: oneshot::Sender<Result<OutgoingCallsResult, String>>,
    },
    SignatureHelp {
        file_path: String,
        line: u32,
        character: u32,
        reply: oneshot::Sender<Result<SignatureHelpResult, String>>,
    },
    InlayHints {
        file_path: String,
        start_line: u32,
        start_character: u32,
        end_line: u32,
        end_character: u32,
        reply: oneshot::Sender<Result<InlayHintsResult, String>>,
    },
    GoToImplementation {
        file_path: String,
        line: u32,
        character: u32,
        reply: oneshot::Sender<Result<LocationsResult, String>>,
    },
    GoToTypeDefinition {
        file_path: String,
        line: u32,
        character: u32,
        reply: oneshot::Sender<Result<LocationsResult, String>>,
    },
    CachedDiagnostics {
        file_path: String,
        reply: oneshot::Sender<Result<DiagnosticsResult, String>>,
    },
    HasCachedDiagnostics {
        file_path: String,
        reply: oneshot::Sender<Result<bool, String>>,
    },
    OpenDocumentPaths {
        reply: oneshot::Sender<Vec<PathBuf>>,
    },
    ValidatePath {
        file_path: String,
        reply: oneshot::Sender<Result<(), String>>,
    },
    AddWorkspaceRoot {
        root: PathBuf,
        reply: oneshot::Sender<Result<ProjectState, String>>,
    },
    StoreEditPlan {
        plan: EditPlan,
        reply: oneshot::Sender<Result<(), String>>,
    },
    TakeEditPlan {
        plan_id: PlanId,
        project_id: String,
        reply: oneshot::Sender<Result<EditPlan, String>>,
    },
    ApplyEditPlan {
        plan_id: PlanId,
        project_id: String,
        root: PathBuf,
        session_id: Option<String>,
        principal: Option<String>,
        reply: oneshot::Sender<Result<AppliedEditPlan, String>>,
    },
    PublishEvent {
        event: ProjectEvent,
        reply: oneshot::Sender<()>,
    },
    PreviewEdit {
        project_id: String,
        edit: WorkspaceEdit,
        encoding: PositionEncoding,
        root: PathBuf,
        reply: oneshot::Sender<Result<PreviewArtifact, String>>,
    },
    MoveInlineModulePreview {
        project_id: String,
        file_path: String,
        module_name: String,
        module_position: Option<lsp_types::Position>,
        encoding: PositionEncoding,
        root: PathBuf,
        reply: oneshot::Sender<Result<PreviewArtifact, String>>,
    },
    StructuralReplacePreview {
        project_id: String,
        request: StructuralReplaceRequest,
        root: PathBuf,
        reply: oneshot::Sender<Result<StructuralPreview, String>>,
    },
    PathRenamePreview {
        project_id: String,
        request: PathRenameRequest,
        root: PathBuf,
        reply: oneshot::Sender<Result<PathRenamePreview, String>>,
    },
    ServerLogs {
        limit: usize,
        min_level: Option<String>,
        reply: oneshot::Sender<Result<ServerLogsResult, String>>,
    },
    ServerMessages {
        limit: usize,
        reply: oneshot::Sender<Result<ServerMessagesResult, String>>,
    },
    ServerCapabilities {
        language_id: Option<String>,
        reply: oneshot::Sender<Result<Vec<ServerCapability>, String>>,
    },
    Notification {
        generation: u64,
        server_id: ServerId,
        notification: LspNotification,
    },
    ServerExited {
        generation: u64,
    },
    Restart {
        reply: oneshot::Sender<ProjectState>,
    },
    Fail {
        message: String,
        reply: oneshot::Sender<()>,
    },
    Shutdown {
        reply: oneshot::Sender<()>,
    },
    Suspend {
        reply: oneshot::Sender<Result<(), ()>>,
    },
}

impl ProjectRequest {
    fn into_resident(self) -> (Self, Option<residency::RustResidencyGuard>) {
        match self {
            Self::Resident { request, guard } => (*request, Some(guard)),
            request => (request, None),
        }
    }

    const fn uses_rust_residency(&self) -> bool {
        matches!(
            self,
            Self::Activate { .. }
                | Self::ActivateWorkspaceRoots { .. }
                | Self::Hover { .. }
                | Self::Definition { .. }
                | Self::References { .. }
                | Self::Diagnostics { .. }
                | Self::Rename { .. }
                | Self::RenameWorkspaceEdit { .. }
                | Self::Completions { .. }
                | Self::DocumentSymbols { .. }
                | Self::FormatDocument { .. }
                | Self::FormatWorkspaceEdit { .. }
                | Self::RangeFormatWorkspaceEdit { .. }
                | Self::MoveItemWorkspaceEdit { .. }
                | Self::SemanticDiscovery { .. }
                | Self::WorkspaceSymbol { .. }
                | Self::CodeActions { .. }
                | Self::CodeActionList { .. }
                | Self::PrepareCallHierarchy { .. }
                | Self::IncomingCalls { .. }
                | Self::OutgoingCalls { .. }
                | Self::SignatureHelp { .. }
                | Self::InlayHints { .. }
                | Self::GoToImplementation { .. }
                | Self::GoToTypeDefinition { .. }
                | Self::AddWorkspaceRoot { .. }
                | Self::ApplyEditPlan { .. }
                | Self::MoveInlineModulePreview { .. }
                | Self::PathRenamePreview { .. }
                | Self::Restart { .. }
                | Self::ServerExited { .. }
        ) || matches!(
            self,
            Self::StructuralReplacePreview {
                request: StructuralReplaceRequest {
                    dialect: StructuralDialect::RustAnalyzerSsr,
                    ..
                },
                ..
            }
        )
    }

    const fn resumes_rust_runtime(&self) -> bool {
        self.uses_rust_residency()
            && !matches!(
                self,
                Self::Activate { .. }
                    | Self::ActivateWorkspaceRoots { .. }
                    | Self::AddWorkspaceRoot { .. }
                    | Self::Restart { .. }
                    | Self::ServerExited { .. }
            )
    }

    /// Fail LSP work that was queued while this actor exhausted recovery.
    ///
    /// Inspection and lifecycle requests still pass through so callers can
    /// observe the failure and explicitly reactivate the project.
    fn reject_if_failed(self, status: ProjectStatus) -> Result<Self, ()> {
        if status != ProjectStatus::Failed {
            return Ok(self);
        }

        macro_rules! reject {
            ($reply:expr) => {{
                let _ = $reply.send(Err(LANGUAGE_SERVER_EXITED.to_string()));
                return Err(());
            }};
        }

        match self {
            Self::Hover { reply, .. } => reject!(reply),
            Self::Definition { reply, .. } => reject!(reply),
            Self::References { reply, .. } => reject!(reply),
            Self::Diagnostics { reply, .. } => reject!(reply),
            Self::Rename { reply, .. } => reject!(reply),
            Self::RenameWorkspaceEdit { reply, .. } | Self::FormatWorkspaceEdit { reply, .. } => {
                reject!(reply)
            }
            Self::RangeFormatWorkspaceEdit { reply, .. }
            | Self::MoveItemWorkspaceEdit { reply, .. } => reject!(reply),
            Self::SemanticDiscovery { reply, .. } => reject!(reply),
            Self::Completions { reply, .. } => reject!(reply),
            Self::DocumentSymbols { reply, .. } => reject!(reply),
            Self::FormatDocument { reply, .. } => reject!(reply),
            // Workspace-symbol lookup has an in-process AST fallback, so it
            // remains available even after all configured LSPs fail.
            Self::CodeActions { reply, .. } | Self::CodeActionList { reply, .. } => {
                reject!(reply)
            }
            Self::PrepareCallHierarchy { reply, .. } => reject!(reply),
            Self::IncomingCalls { reply, .. } => reject!(reply),
            Self::OutgoingCalls { reply, .. } => reject!(reply),
            Self::SignatureHelp { reply, .. } => reject!(reply),
            Self::InlayHints { reply, .. } => reject!(reply),
            Self::GoToImplementation { reply, .. } | Self::GoToTypeDefinition { reply, .. } => {
                reject!(reply)
            }
            request => Ok(request),
        }
    }
}

impl ProjectRequest {
    fn is_cancelled(&self) -> bool {
        match self {
            Self::Resident { request, .. } => request.is_cancelled(),
            Self::Query { reply } | Self::Refresh { reply } | Self::Restart { reply } => {
                reply.is_closed()
            }
            Self::SetStatus { reply, .. } | Self::Fail { reply, .. } => reply.is_closed(),
            Self::Activate { reply, .. } | Self::ActivateWorkspaceRoots { reply, .. } => {
                reply.is_closed()
            }
            Self::Hover { reply, .. } => reply.is_closed(),
            Self::Definition { reply, .. } => reply.is_closed(),
            Self::References { reply, .. } => reply.is_closed(),
            Self::Diagnostics { reply, .. } | Self::CachedDiagnostics { reply, .. } => {
                reply.is_closed()
            }
            Self::Rename { reply, .. } => reply.is_closed(),
            Self::RenameWorkspaceEdit { reply, .. } | Self::FormatWorkspaceEdit { reply, .. } => {
                reply.is_closed()
            }
            Self::RangeFormatWorkspaceEdit { reply, .. }
            | Self::MoveItemWorkspaceEdit { reply, .. } => reply.is_closed(),
            Self::SemanticDiscovery { reply, .. } => reply.is_closed(),
            Self::Completions { reply, .. } => reply.is_closed(),
            Self::DocumentSymbols { reply, .. } => reply.is_closed(),
            Self::FormatDocument { reply, .. } => reply.is_closed(),
            Self::WorkspaceSymbol { reply, .. } => reply.is_closed(),
            Self::CodeActions { reply, .. } | Self::CodeActionList { reply, .. } => {
                reply.is_closed()
            }
            Self::CodeActionPreview { reply, .. }
            | Self::PreviewEdit { reply, .. }
            | Self::MoveInlineModulePreview { reply, .. } => reply.is_closed(),
            Self::StructuralReplacePreview { reply, .. } => reply.is_closed(),
            Self::PathRenamePreview { reply, .. } => reply.is_closed(),
            Self::PrepareCallHierarchy { reply, .. } => reply.is_closed(),
            Self::IncomingCalls { reply, .. } => reply.is_closed(),
            Self::OutgoingCalls { reply, .. } => reply.is_closed(),
            Self::SignatureHelp { reply, .. } => reply.is_closed(),
            Self::InlayHints { reply, .. } => reply.is_closed(),
            Self::GoToImplementation { reply, .. } | Self::GoToTypeDefinition { reply, .. } => {
                reply.is_closed()
            }
            Self::HasCachedDiagnostics { reply, .. } => reply.is_closed(),
            Self::OpenDocumentPaths { reply } => reply.is_closed(),
            Self::ValidatePath { reply, .. } | Self::StoreEditPlan { reply, .. } => {
                reply.is_closed()
            }
            Self::AddWorkspaceRoot { reply, .. } => reply.is_closed(),
            Self::TakeEditPlan { reply, .. } => reply.is_closed(),
            Self::ApplyEditPlan { reply, .. } => reply.is_closed(),
            Self::ServerLogs { reply, .. } => reply.is_closed(),
            Self::ServerMessages { reply, .. } => reply.is_closed(),
            Self::ServerCapabilities { reply, .. } => reply.is_closed(),
            Self::PublishEvent { .. }
            | Self::Shutdown { .. }
            | Self::Suspend { .. }
            | Self::Notification { .. }
            | Self::ServerExited { .. } => false,
        }
    }
}

/// Cloneable handle for querying and controlling one project actor.
#[derive(Clone)]
pub struct ProjectHandle {
    sender: ProjectRequestSender,
    status: watch::Receiver<ProjectStatus>,
    events: broadcast::Sender<ProjectEvent>,
    event_history: std::sync::Arc<std::sync::Mutex<ProjectEventHistory>>,
}

impl ProjectHandle {
    /// Return the actor request queue depth and fixed capacity without awaiting it.
    #[must_use]
    pub fn queue_pressure(&self) -> ProjectQueuePressure {
        self.sender.queue_pressure()
    }

    /// Subscribe to lifecycle changes for this project.
    #[must_use]
    pub fn status(&self) -> watch::Receiver<ProjectStatus> {
        self.status.clone()
    }

    /// Subscribe to typed project lifecycle and failure events.
    #[must_use]
    pub fn subscribe_events(&self) -> broadcast::Receiver<ProjectEvent> {
        self.events.subscribe()
    }

    /// Return retained project events newer than an optional polling cursor.
    #[must_use]
    pub fn event_snapshot(&self, cursor: Option<u64>) -> ProjectEventSnapshot {
        self.event_history
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .snapshot_since(cursor)
    }

    fn reject_new_work(&self) {
        self.sender.reject_new_work();
    }

    fn accept_new_work(&self) {
        self.sender.accept_new_work();
    }

    async fn publish_event(&self, event: ProjectEvent) -> Result<(), ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send_unchecked(ProjectRequest::PublishEvent { event, reply })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response.await.map_err(|_| ProjectActorError::Cancelled)
    }

    /// Query the actor's current state.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor has stopped or drops the response.
    pub async fn query(&self) -> Result<ProjectState, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::Query { reply })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response.await.map_err(|_| ProjectActorError::Cancelled)
    }

    /// Change the actor's observable lifecycle state.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor has stopped or drops the response.
    pub async fn set_status(&self, status: ProjectStatus) -> Result<(), ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::SetStatus { status, reply })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response.await.map_err(|_| ProjectActorError::Cancelled)
    }

    /// Refresh the actor's current state without mutating it.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor has stopped or drops the response.
    pub async fn refresh(&self) -> Result<ProjectState, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::Refresh { reply })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response.await.map_err(|_| ProjectActorError::Cancelled)
    }

    /// Activate the actor-owned language servers for its project root.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor is closed or language-server activation
    /// fails.
    pub async fn activate(&self, root: PathBuf) -> Result<ProjectState, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::Activate { root, reply })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    /// Activate the actor-owned language servers for all linked workspace roots.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor is closed or language-server activation
    /// fails.
    pub async fn activate_workspace_roots(
        &self,
        roots: Vec<PathBuf>,
    ) -> Result<ProjectState, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::ActivateWorkspaceRoots { roots, reply })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    /// Route a hover request through this project's actor-owned translator.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor is closed, cancels the response, or the
    /// actor-owned translator rejects the request.
    pub async fn hover(
        &self,
        file_path: String,
        line: u32,
        character: u32,
    ) -> Result<HoverResult, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::Hover {
                file_path,
                line,
                character,
                reply,
            })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    /// Route a definition request through this project's actor-owned translator.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor is closed, cancels the response, or the
    /// actor-owned translator rejects the request.
    pub async fn definition(
        &self,
        file_path: String,
        line: u32,
        character: u32,
    ) -> Result<DefinitionResult, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::Definition {
                file_path,
                line,
                character,
                reply,
            })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    /// Route a references request through this project's actor-owned translator.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor is closed, cancels the response, or the
    /// actor-owned translator rejects the request.
    pub async fn references(
        &self,
        file_path: String,
        line: u32,
        character: u32,
        include_declaration: bool,
    ) -> Result<ReferencesResult, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::References {
                file_path,
                line,
                character,
                include_declaration,
                reply,
            })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    /// Route a diagnostics request through this project's actor-owned translator.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor is closed, cancels the response, or the
    /// actor-owned translator rejects the request.
    pub async fn diagnostics(
        &self,
        file_path: String,
    ) -> Result<DiagnosticsResult, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::Diagnostics { file_path, reply })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    /// Route a rename request through this project's actor-owned translator.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor is closed, cancels the response, or the
    /// actor-owned translator rejects the request.
    pub async fn rename(
        &self,
        file_path: String,
        line: u32,
        character: u32,
        new_name: String,
    ) -> Result<RenameResult, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::Rename {
                file_path,
                line,
                character,
                new_name,
                reply,
            })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    /// Request a raw LSP workspace edit for a rename.
    ///
    /// # Errors
    ///
    /// Returns an error when the actor is closed, the request is cancelled, or
    /// the actor-owned translator rejects the request.
    pub async fn rename_workspace_edit(
        &self,
        file_path: String,
        line: u32,
        character: u32,
        new_name: String,
    ) -> Result<Option<WorkspaceEdit>, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::RenameWorkspaceEdit {
                file_path,
                line,
                character,
                new_name,
                reply,
            })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    /// Route a completion request through this project's actor-owned translator.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor is closed, cancels the response, or the
    /// actor-owned translator rejects the request.
    pub async fn completions(
        &self,
        file_path: String,
        line: u32,
        character: u32,
        trigger: Option<String>,
    ) -> Result<CompletionsResult, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::Completions {
                file_path,
                line,
                character,
                trigger,
                reply,
            })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    /// Route a document-symbol request through this project's actor-owned translator.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor is closed, cancels the response, or the
    /// actor-owned translator rejects the request.
    pub async fn document_symbols(
        &self,
        file_path: String,
    ) -> Result<DocumentSymbolsResult, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::DocumentSymbols { file_path, reply })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    /// Route a document-formatting request through this project's actor-owned translator.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor is closed, cancels the response, or the
    /// actor-owned translator rejects the request.
    pub async fn format_document(
        &self,
        file_path: String,
        tab_size: u32,
        insert_spaces: bool,
    ) -> Result<FormatDocumentResult, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::FormatDocument {
                file_path,
                tab_size,
                insert_spaces,
                reply,
            })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    /// Request a raw LSP workspace edit for document formatting.
    ///
    /// # Errors
    ///
    /// Returns an error when the actor is closed, the request is cancelled, or
    /// the actor-owned translator rejects the request.
    pub async fn format_workspace_edit(
        &self,
        file_path: String,
        tab_size: u32,
        insert_spaces: bool,
    ) -> Result<Option<WorkspaceEdit>, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::FormatWorkspaceEdit {
                file_path,
                tab_size,
                insert_spaces,
                reply,
            })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    pub(crate) async fn range_format_workspace_edit(
        &self,
        file_path: String,
        start: (u32, u32),
        end: (u32, u32),
        tab_size: u32,
        insert_spaces: bool,
    ) -> Result<SupportedWorkspaceEdit, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::RangeFormatWorkspaceEdit {
                file_path,
                start,
                end,
                tab_size,
                insert_spaces,
                reply,
            })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    pub(crate) async fn move_item_workspace_edit(
        &self,
        file_path: String,
        start: (u32, u32),
        end: (u32, u32),
        direction: String,
    ) -> Result<SupportedWorkspaceEdit, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::MoveItemWorkspaceEdit {
                file_path,
                start,
                end,
                direction,
                reply,
            })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    pub(crate) async fn semantic_discovery(
        &self,
        file_path: String,
        line: u32,
        character: u32,
        kind: SemanticDiscoveryKind,
    ) -> Result<SemanticDiscoveryResult, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::SemanticDiscovery {
                file_path,
                line,
                character,
                kind,
                reply,
            })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    /// Route a workspace-symbol request through this project's actor-owned translator.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor is closed, cancels the response, or the
    /// actor-owned translator rejects the request.
    pub async fn workspace_symbol(
        &self,
        query: String,
        kind_filter: Option<String>,
        limit: u32,
    ) -> Result<WorkspaceSymbolResult, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::WorkspaceSymbol {
                query,
                kind_filter,
                limit,
                reply,
            })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    /// Route a code-action request through this project's actor-owned translator.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor is closed, cancels the response, or the
    /// actor-owned translator rejects the request.
    pub async fn code_actions(
        &self,
        file_path: String,
        start_line: u32,
        start_character: u32,
        end_line: u32,
        end_character: u32,
        kind_filter: Option<String>,
    ) -> Result<CodeActionsResult, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::CodeActions {
                file_path,
                start_line,
                start_character,
                end_line,
                end_character,
                kind_filter,
                reply,
            })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    /// List code actions and retain bounded project-local references.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor is unavailable or the language server
    /// rejects the request.
    pub async fn code_action_list(
        &self,
        file_path: String,
        start_line: u32,
        start_character: u32,
        end_line: u32,
        end_character: u32,
        kind_filter: Option<String>,
    ) -> Result<CodeActionsResult, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::CodeActionList {
                file_path,
                start_line,
                start_character,
                end_line,
                end_character,
                kind_filter,
                reply,
            })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    /// Preview one retained code action reference.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor is unavailable, the action is stale, or
    /// its command/edit cannot be safely previewed.
    pub async fn preview_code_action(
        &self,
        action_id: PlanId,
        project_id: String,
        encoding: PositionEncoding,
        root: PathBuf,
    ) -> Result<PreviewArtifact, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::CodeActionPreview {
                action_id,
                project_id,
                encoding,
                root,
                reply,
            })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    /// Route call-hierarchy preparation through this project's actor-owned translator.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor is closed, cancels the response, or the
    /// actor-owned translator rejects the request.
    pub async fn prepare_call_hierarchy(
        &self,
        file_path: String,
        line: u32,
        character: u32,
    ) -> Result<CallHierarchyPrepareResult, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::PrepareCallHierarchy {
                file_path,
                line,
                character,
                reply,
            })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    /// Route incoming call hierarchy requests through this project's actor-owned translator.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor is closed, cancels the response, or the
    /// actor-owned translator rejects the request.
    pub async fn incoming_calls(
        &self,
        item: serde_json::Value,
    ) -> Result<IncomingCallsResult, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::IncomingCalls { item, reply })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    /// Route outgoing call hierarchy requests through this project's actor-owned translator.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor is closed, cancels the response, or the
    /// actor-owned translator rejects the request.
    pub async fn outgoing_calls(
        &self,
        item: serde_json::Value,
    ) -> Result<OutgoingCallsResult, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::OutgoingCalls { item, reply })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    /// Route signature help through this project's actor-owned translator.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor is closed, cancels the response, or the
    /// actor-owned translator rejects the request.
    pub async fn signature_help(
        &self,
        file_path: String,
        line: u32,
        character: u32,
    ) -> Result<SignatureHelpResult, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::SignatureHelp {
                file_path,
                line,
                character,
                reply,
            })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    /// Route inlay hints through this project's actor-owned translator.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor is closed, cancels the response, or the
    /// actor-owned translator rejects the request.
    pub async fn inlay_hints(
        &self,
        file_path: String,
        start_line: u32,
        start_character: u32,
        end_line: u32,
        end_character: u32,
    ) -> Result<InlayHintsResult, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::InlayHints {
                file_path,
                start_line,
                start_character,
                end_line,
                end_character,
                reply,
            })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    /// Route implementation lookup through this project's actor-owned translator.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor is closed, cancels the response, or the
    /// actor-owned translator rejects the request.
    pub async fn go_to_implementation(
        &self,
        file_path: String,
        line: u32,
        character: u32,
    ) -> Result<LocationsResult, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::GoToImplementation {
                file_path,
                line,
                character,
                reply,
            })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    /// Route type-definition lookup through this project's actor-owned translator.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor is closed, cancels the response, or the
    /// actor-owned translator rejects the request.
    pub async fn go_to_type_definition(
        &self,
        file_path: String,
        line: u32,
        character: u32,
    ) -> Result<LocationsResult, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::GoToTypeDefinition {
                file_path,
                line,
                character,
                reply,
            })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    /// Route cached diagnostics through this project's actor-owned translator.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor is closed, cancels the response, or the
    /// actor-owned translator rejects the request.
    pub async fn cached_diagnostics(
        &self,
        file_path: String,
    ) -> Result<DiagnosticsResult, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::CachedDiagnostics { file_path, reply })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    /// Return whether cached diagnostics exist for a document path.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor is closed, cancels the response, or the
    /// actor-owned translator rejects the request.
    pub async fn has_cached_diagnostics(
        &self,
        file_path: String,
    ) -> Result<bool, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::HasCachedDiagnostics { file_path, reply })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    /// Return the paths of documents currently owned by this actor.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor has stopped before replying.
    pub async fn open_document_paths(&self) -> Result<Vec<PathBuf>, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::OpenDocumentPaths { reply })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response.await.map_err(|_| ProjectActorError::Cancelled)
    }

    /// Validate that a path belongs to this project's workspace.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor is closed, cancels the response, or the
    /// path is outside the actor-owned workspace roots.
    pub async fn validate_path(&self, file_path: String) -> Result<(), ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::ValidatePath { file_path, reply })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    /// Add a compatible linked-project root to this actor's workspace.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor is closed, cancels the response, or the
    /// language servers cannot be restarted with the expanded root set.
    pub async fn add_workspace_root(
        &self,
        root: PathBuf,
    ) -> Result<ProjectState, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::AddWorkspaceRoot { root, reply })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    /// Store a project-owned workspace edit preview for later application.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor is closed, cancels the response, or the
    /// bounded plan store rejects the plan.
    pub async fn store_edit_plan(&self, plan: EditPlan) -> Result<(), ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::StoreEditPlan { plan, reply })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    /// Preview and store one project-owned LSP workspace edit.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor is closed, the edit cannot be safely
    /// planned, or the bounded plan store rejects the resulting artifact.
    pub async fn preview_edit(
        &self,
        project_id: String,
        edit: WorkspaceEdit,
        encoding: PositionEncoding,
        root: PathBuf,
    ) -> Result<PreviewArtifact, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::PreviewEdit {
                project_id,
                edit,
                encoding,
                root,
                reply,
            })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    /// Build and store an inline Rust module move from actor-owned document state.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor is closed, the request is cancelled, or
    /// the project-owned refactor/preview rejects the operation.
    pub async fn move_inline_module_preview(
        &self,
        project_id: String,
        file_path: String,
        module_name: String,
        module_position: Option<lsp_types::Position>,
        encoding: PositionEncoding,
        root: PathBuf,
    ) -> Result<PreviewArtifact, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::MoveInlineModulePreview {
                project_id,
                file_path,
                module_name,
                module_position,
                encoding,
                root,
                reply,
            })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    /// Search or preview one explicitly selected structural replacement dialect.
    pub(crate) async fn structural_replace_preview(
        &self,
        project_id: String,
        request: StructuralReplaceRequest,
        root: PathBuf,
    ) -> Result<StructuralPreview, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::StructuralReplacePreview {
                project_id,
                request,
                root,
                reply,
            })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    pub(crate) async fn path_rename_preview(
        &self,
        project_id: String,
        request: PathRenameRequest,
        root: PathBuf,
    ) -> Result<PathRenamePreview, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::PathRenamePreview {
                project_id,
                request,
                root,
                reply,
            })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    /// Consume one project-owned workspace edit preview.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor is closed, cancels the response, or the
    /// plan is missing, expired, or owned by another project.
    pub async fn take_edit_plan(
        &self,
        plan_id: PlanId,
        project_id: String,
    ) -> Result<EditPlan, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::TakeEditPlan {
                plan_id,
                project_id,
                reply,
            })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    /// Consume and apply one project-owned workspace edit preview.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor is closed, the plan is not owned by the
    /// project, or filesystem validation/application fails.
    pub async fn apply_edit_plan(
        &self,
        plan_id: PlanId,
        project_id: String,
        root: PathBuf,
    ) -> Result<AppliedEditPlan, ProjectActorError> {
        self.apply_edit_plan_with_context(plan_id, project_id, root, None, None)
            .await
    }

    /// Consume and apply one project-owned workspace edit preview with audit context.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor is closed, the plan is not owned by the
    /// project, or filesystem validation/application fails.
    pub async fn apply_edit_plan_with_context(
        &self,
        plan_id: PlanId,
        project_id: String,
        root: PathBuf,
        session_id: Option<String>,
        principal: Option<String>,
    ) -> Result<AppliedEditPlan, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::ApplyEditPlan {
                plan_id,
                project_id,
                root,
                session_id,
                principal,
                reply,
            })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    /// Return recent logs from this project's language servers.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor is closed, cancels the response, or the
    /// requested log filter is invalid.
    pub async fn server_logs(
        &self,
        limit: usize,
        min_level: Option<String>,
    ) -> Result<ServerLogsResult, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::ServerLogs {
                limit,
                min_level,
                reply,
            })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    /// Return recent messages from this project's language servers.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor is closed or cancels the response.
    pub async fn server_messages(
        &self,
        limit: usize,
    ) -> Result<ServerMessagesResult, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::ServerMessages { limit, reply })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    async fn server_logs_unchecked(
        &self,
        limit: usize,
        min_level: Option<String>,
    ) -> Result<ServerLogsResult, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send_unchecked(ProjectRequest::ServerLogs {
                limit,
                min_level,
                reply,
            })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    async fn server_messages_unchecked(
        &self,
        limit: usize,
    ) -> Result<ServerMessagesResult, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send_unchecked(ProjectRequest::ServerMessages { limit, reply })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    async fn server_capabilities_unchecked(
        &self,
        language_id: Option<String>,
    ) -> Result<Vec<ServerCapability>, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send_unchecked(ProjectRequest::ServerCapabilities { language_id, reply })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    /// Return negotiated capabilities for this project's active language servers.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor is closed or cancels the response.
    pub async fn server_capabilities(
        &self,
        language_id: Option<String>,
    ) -> Result<Vec<ServerCapability>, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::ServerCapabilities { language_id, reply })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    /// Restart the project actor's managed services.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor has stopped or drops the response.
    pub async fn restart(&self) -> Result<ProjectState, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::Restart { reply })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response.await.map_err(|_| ProjectActorError::Cancelled)
    }

    /// Record a failure and expose it through [`ProjectState::last_error`].
    ///
    /// # Errors
    ///
    /// Returns an error if the actor has stopped or drops the response.
    pub async fn fail(&self, message: impl Into<String>) -> Result<(), ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::Fail {
                message: message.into(),
                reply,
            })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response.await.map_err(|_| ProjectActorError::Cancelled)
    }

    /// Stop the actor after publishing `Stopped`.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor has already stopped or drops the response.
    pub async fn shutdown(&self) -> Result<(), ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send_unchecked(ProjectRequest::Shutdown { reply })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response.await.map_err(|_| ProjectActorError::Cancelled)
    }
}

struct ProjectRuntime {
    translator: Translator,
    edit_plans: EditPlanStore,
    edit_safety: Option<EditSafetyConfig>,
    code_actions: CodeActionStore,
    inline_module_checks: HashMap<PlanId, InlineModuleSemanticCheck>,
    generation: u64,
    automatic_restart: AutomaticRestartPolicy,
}

#[derive(Debug, Clone)]
struct InlineModuleSemanticCheck {
    source_path: PathBuf,
    destination_path: PathBuf,
    module_name: String,
    source_position: lsp_types::Position,
    pre_verification: VerificationStatus,
}

const LANGUAGE_SERVER_EXITED: &str = "language server exited";
const MAX_AUTOMATIC_RESTART_ATTEMPTS: usize = 3;
const MAX_INLINE_MODULE_CHECKS: usize = 256;
const AUTOMATIC_RESTART_BACKOFF: [Duration; MAX_AUTOMATIC_RESTART_ATTEMPTS] = [
    Duration::from_millis(100),
    Duration::from_millis(500),
    Duration::from_secs(2),
];

const fn position_in_mcp_range(line: u32, character: u32, range: &crate::bridge::Range) -> bool {
    let after_start =
        line > range.start.line || (line == range.start.line && character >= range.start.character);
    let before_end =
        line < range.end.line || (line == range.end.line && character <= range.end.character);
    after_start && before_end
}

fn structural_matches_from_workspace_edit(
    edit: &WorkspaceEdit,
) -> Result<Vec<StructuralMatch>, String> {
    let normalized = normalize(edit.clone()).expect("workspace edit normalization is infallible");
    let mut matches = Vec::new();
    for operation in normalized.operations {
        let EditOperation::Text { uri, edits, .. } = operation else {
            return Err("rust-analyzer SSR returned an unsupported resource operation".to_string());
        };
        let path = uri_to_path(&uri)
            .ok_or_else(|| "rust-analyzer SSR returned a non-file URI".to_string())?;
        if matches.len().saturating_add(edits.len()) > PreviewLimits::default().max_edits {
            return Err("rust-analyzer SSR exceeded the structural match limit".to_string());
        }
        matches.extend(edits.into_iter().map(|edit| StructuralMatch {
            path: path.clone(),
            range: edit.range,
        }));
    }
    matches.sort_by(|left, right| {
        left.path.cmp(&right.path).then_with(|| {
            (
                left.range.start.line,
                left.range.start.character,
                left.range.end.line,
                left.range.end.character,
            )
                .cmp(&(
                    right.range.start.line,
                    right.range.start.character,
                    right.range.end.line,
                    right.range.end.character,
                ))
        })
    });
    Ok(matches)
}

#[allow(clippy::mutable_key_type)]
fn compose_path_rename_edit(
    result: WillRenameFilesResult,
    old_path: &Path,
    new_path: &Path,
) -> Result<(WorkspaceEdit, Vec<String>, usize), String> {
    let mut changes = HashMap::new();
    let mut operations = Vec::new();
    let mut annotations = HashMap::new();
    let mut semantic_edit_count = 0usize;
    for edit in result.edits {
        for (uri, edits) in edit.changes.unwrap_or_default() {
            semantic_edit_count = semantic_edit_count.saturating_add(edits.len());
            changes.entry(uri).or_insert_with(Vec::new).extend(edits);
        }
        if let Some(document_changes) = edit.document_changes {
            match document_changes {
                lsp_types::DocumentChanges::Edits(edits) => {
                    semantic_edit_count = semantic_edit_count
                        .saturating_add(edits.iter().map(|edit| edit.edits.len()).sum::<usize>());
                    operations.extend(
                        edits
                            .into_iter()
                            .map(lsp_types::DocumentChangeOperation::Edit),
                    );
                }
                lsp_types::DocumentChanges::Operations(returned) => {
                    for operation in returned {
                        let lsp_types::DocumentChangeOperation::Edit(edit) = operation else {
                            return Err(
                                "workspace/willRenameFiles returned a resource operation; MCPLS adds exactly one requested RenameFile"
                                    .to_string(),
                            );
                        };
                        semantic_edit_count = semantic_edit_count.saturating_add(edit.edits.len());
                        operations.push(lsp_types::DocumentChangeOperation::Edit(edit));
                    }
                }
            }
        }
        for (id, annotation) in edit.change_annotations.unwrap_or_default() {
            if annotations
                .insert(id.clone(), annotation.clone())
                .is_some_and(|existing| existing != annotation)
            {
                return Err(format!(
                    "workspace/willRenameFiles returned conflicting annotation {id}"
                ));
            }
        }
    }
    operations.push(lsp_types::DocumentChangeOperation::Op(
        lsp_types::ResourceOp::Rename(lsp_types::RenameFile {
            old_uri: path_to_uri(old_path).map_err(|error| error.to_string())?,
            new_uri: path_to_uri(new_path).map_err(|error| error.to_string())?,
            options: None,
            annotation_id: None,
        }),
    ));
    Ok((
        WorkspaceEdit {
            changes: (!changes.is_empty()).then_some(changes),
            document_changes: Some(lsp_types::DocumentChanges::Operations(operations)),
            change_annotations: (!annotations.is_empty()).then_some(annotations),
        },
        result.providers,
        semantic_edit_count,
    ))
}

#[derive(Debug, Default)]
struct AutomaticRestartPolicy {
    attempts: usize,
}

impl AutomaticRestartPolicy {
    fn next(&mut self) -> Option<AutomaticRestartAttempt> {
        let attempt = self.attempts + 1;
        let delay = AUTOMATIC_RESTART_BACKOFF.get(self.attempts).copied()?;
        self.attempts = attempt;
        Some(AutomaticRestartAttempt {
            number: attempt,
            delay,
        })
    }

    const fn reset(&mut self) {
        self.attempts = 0;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AutomaticRestartAttempt {
    number: usize,
    delay: Duration,
}

struct StoredCodeAction {
    file_path: String,
    action: lsp_types::CodeActionOrCommand,
    created_at: Instant,
}

struct CodeActionStore {
    entries: HashMap<PlanId, StoredCodeAction>,
    ttl: Duration,
    max_entries: usize,
}

impl CodeActionStore {
    fn new() -> Self {
        Self {
            entries: HashMap::new(),
            ttl: Duration::from_secs(15 * 60),
            max_entries: 256,
        }
    }

    fn prune(&mut self) {
        let now = Instant::now();
        self.entries
            .retain(|_, entry| now.duration_since(entry.created_at) < self.ttl);
    }

    fn enforce_capacity(&mut self) {
        while self.entries.len() >= self.max_entries {
            let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.created_at)
                .map(|(id, _)| id.clone())
            else {
                break;
            };
            self.entries.remove(&oldest);
        }
    }

    fn insert(&mut self, action: StoredCodeAction) -> PlanId {
        self.prune();
        self.enforce_capacity();
        let id = PlanId::new();
        self.entries.insert(id.clone(), action);
        id
    }

    fn take(&mut self, id: &PlanId) -> Result<StoredCodeAction, String> {
        self.prune();
        self.entries
            .remove(id)
            .ok_or_else(|| format!("code action reference is missing or expired: {id}"))
    }
}

impl ProjectRuntime {
    fn new(translator: Translator) -> Self {
        Self::with_edit_safety(translator, None)
    }

    fn with_edit_safety(translator: Translator, edit_safety: Option<EditSafetyConfig>) -> Self {
        Self {
            translator,
            edit_plans: EditPlanStore::for_project(),
            edit_safety,
            code_actions: CodeActionStore::new(),
            inline_module_checks: HashMap::new(),
            generation: 0,
            automatic_restart: AutomaticRestartPolicy::default(),
        }
    }

    const fn begin_transition(&mut self) {
        self.generation = self.generation.wrapping_add(1);
    }

    const fn generation(&self) -> u64 {
        self.generation
    }

    const fn owns_generation(&self, generation: u64) -> bool {
        self.generation == generation
    }

    fn has_active_workspace_roots(&self, roots: &[PathBuf]) -> bool {
        self.translator.has_active_workspace_roots(roots)
    }

    fn activation_is_reusable(&self, status: ProjectStatus, roots: &[PathBuf]) -> bool {
        matches!(
            status,
            ProjectStatus::Starting | ProjectStatus::Ready | ProjectStatus::Degraded
        ) && self.has_active_workspace_roots(roots)
    }

    fn begin_automatic_restart(&mut self) -> Option<AutomaticRestartAttempt> {
        let attempt = self.automatic_restart.next()?;
        self.begin_transition();
        Some(attempt)
    }

    const fn reset_automatic_restart(&mut self) {
        self.automatic_restart.reset();
    }

    fn store_edit_plan(&mut self, plan: EditPlan) -> Result<(), String> {
        self.edit_plans
            .insert(plan)
            .map_err(|error| error.to_string())
    }

    fn preview_edit(
        &mut self,
        project_id: &str,
        edit: WorkspaceEdit,
        encoding: PositionEncoding,
        root: &Path,
    ) -> Result<PreviewArtifact, String> {
        let boundary = WorkspaceBoundary::new(root).map_err(|error| error.to_string())?;
        let artifact = preview_workspace_edit(
            &boundary,
            project_id,
            edit,
            encoding,
            self.translator.document_tracker(),
            PreviewLimits::default(),
        )
        .map_err(|error| error.to_string())?;
        self.edit_plans
            .insert(artifact.plan.clone())
            .map_err(|error| error.to_string())?;
        Ok(artifact)
    }

    async fn structural_replace_preview(
        &mut self,
        project_id: &str,
        request: StructuralReplaceRequest,
        root: &Path,
    ) -> Result<StructuralPreview, String> {
        let StructuralReplaceRequest {
            file_path,
            dialect,
            query,
            replacement,
            language_id,
            parse_only,
            encoding,
        } = request;
        self.translator
            .validate_path(Path::new(&file_path))
            .map_err(|error| error.to_string())?;
        let (edit, matches, verification, producer) = match dialect {
            StructuralDialect::RustAnalyzerSsr => {
                if replacement.is_some() || language_id.is_some() {
                    return Err(
                        "rust_analyzer_ssr accepts the complete rust-analyzer rule in query; replacement and language_id must be omitted"
                            .to_string(),
                    );
                }
                let edit = self
                    .translator
                    .request_rust_analyzer_ssr(file_path.clone(), query, parse_only)
                    .await
                    .map_err(|error| error.to_string())?;
                let matches = if parse_only {
                    Vec::new()
                } else {
                    structural_matches_from_workspace_edit(&edit)?
                };
                (
                    (!parse_only && !matches.is_empty()).then_some(edit),
                    matches,
                    VerificationStatus::SemanticVerified,
                    EditProducer::RustAnalyzer,
                )
            }
            StructuralDialect::AstGrep => {
                let language = language_id.ok_or_else(|| {
                    "ast_grep requires an explicit language_id; syntax is never inferred or translated"
                        .to_string()
                })?;
                let StructuralSearchResult { edit, matches } = self
                    .translator
                    .structural_ast_grep_search(
                        root.to_path_buf(),
                        language,
                        query,
                        replacement,
                        encoding,
                        parse_only,
                    )
                    .await?;
                (
                    edit.filter(|_| !matches.is_empty()),
                    matches,
                    VerificationStatus::StructuralUnverified,
                    EditProducer::StructuralAstGrep,
                )
            }
        };
        let artifact = edit
            .map(|edit| {
                let mut artifact = self.preview_edit(project_id, edit, encoding, root)?;
                artifact.verification = Some(verification);
                artifact.producer = Some(producer);
                Ok::<_, String>(artifact)
            })
            .transpose()?;
        Ok(StructuralPreview {
            artifact,
            dialect,
            matches,
            parse_only,
        })
    }

    async fn path_rename_preview(
        &mut self,
        project_id: &str,
        request: PathRenameRequest,
        root: &Path,
    ) -> Result<PathRenamePreview, String> {
        let boundary = WorkspaceBoundary::new(root).map_err(|error| error.to_string())?;
        let old_path = boundary
            .validate_existing(&request.old_path)
            .map_err(|error| error.to_string())?;
        let new_path = boundary
            .validate_target(&request.new_path)
            .map_err(|error| error.to_string())?;
        if old_path == boundary.root() {
            return Err("cannot rename the project root".to_string());
        }
        if old_path == new_path {
            return Err("old_path and new_path resolve to the same path".to_string());
        }
        if old_path.is_dir() && new_path.starts_with(&old_path) {
            return Err("cannot rename a directory into itself".to_string());
        }
        boundary
            .validate_operation(&crate::edit_paths::FileOperation::Rename {
                from: old_path.clone(),
                to: new_path.clone(),
                overwrite: false,
            })
            .map_err(|error| error.to_string())?;

        let result = self
            .translator
            .request_will_rename_files(&old_path, &new_path)
            .await
            .map_err(|error| error.to_string())?;
        let (edit, providers, semantic_edit_count) =
            compose_path_rename_edit(result, &old_path, &new_path)?;
        let mut artifact = self.preview_edit(project_id, edit, request.encoding, root)?;
        artifact.verification = Some(if semantic_edit_count > 0 {
            VerificationStatus::SemanticVerified
        } else {
            VerificationStatus::StructuralUnverified
        });
        if !providers.is_empty() {
            artifact.producer = Some(EditProducer::LanguageServerFileOperations);
        }
        Ok(PathRenamePreview {
            artifact,
            providers,
            semantic_edit_count,
        })
    }

    async fn verify_inline_module_before_preview(
        &self,
        source_path: &Path,
        module_name: &str,
        module_position: Option<lsp_types::Position>,
    ) -> Result<(VerificationStatus, Option<lsp_types::Position>), String> {
        if !self.translator.semantic_server_ready_for_file(source_path) {
            return Ok((VerificationStatus::StructuralUnverified, module_position));
        }
        let Ok(symbols) = self
            .translator
            .handle_document_symbols(source_path.display().to_string())
            .await
        else {
            return Ok((VerificationStatus::StructuralUnverified, module_position));
        };
        let matches = symbols
            .symbols
            .iter()
            .filter(|symbol| {
                logical_module_name(&symbol.name) == logical_module_name(module_name)
                    && symbol.kind.eq_ignore_ascii_case("Module")
                    && module_position.is_none_or(|position| {
                        let line = position.line.saturating_add(1);
                        let character = position.character.saturating_add(1);
                        position_in_mcp_range(line, character, &symbol.range)
                    })
            })
            .count();
        if matches != 1 {
            return Err(format!(
                "rust-analyzer did not identify exactly one module `{module_name}` at the requested location"
            ));
        }
        let selected_position = module_position.or_else(|| {
            symbols
                .symbols
                .iter()
                .find(|symbol| {
                    logical_module_name(&symbol.name) == logical_module_name(module_name)
                        && symbol.kind.eq_ignore_ascii_case("Module")
                })
                .map(|symbol| lsp_types::Position {
                    line: symbol.range.start.line.saturating_sub(1),
                    character: symbol.range.start.character.saturating_sub(1),
                })
        });
        Ok((VerificationStatus::SemanticVerified, selected_position))
    }

    async fn move_inline_module_preview(
        &mut self,
        project_id: &str,
        file_path: &str,
        module_name: &str,
        module_position: Option<lsp_types::Position>,
        encoding: PositionEncoding,
        root: &Path,
    ) -> Result<PreviewArtifact, String> {
        let source_path = self
            .translator
            .validate_path(Path::new(file_path))
            .map_err(|error| error.to_string())?;
        let source_override = self
            .translator
            .document_tracker()
            .get(&source_path)
            .map(|document| document.content().to_string());
        let (verification, verified_position) = self
            .verify_inline_module_before_preview(&source_path, module_name, module_position)
            .await?;
        let structural_edit = move_inline_module_preview_with_source(
            &source_path,
            module_name,
            encoding,
            source_override.as_deref(),
            module_position,
        )
        .map_err(|error| error.to_string())?;
        let native_edit = if verification == VerificationStatus::SemanticVerified {
            match verified_position {
                Some(position) => {
                    self.native_inline_module_move_edit(&source_path, position)
                        .await
                }
                None => None,
            }
        } else {
            None
        };
        let (edit, producer) = native_edit
            .map_or((structural_edit, EditProducer::StructuralAstGrep), |edit| {
                (edit, EditProducer::RustAnalyzer)
            });
        let mut artifact = self.preview_edit(project_id, edit, encoding, root)?;
        artifact.verification = Some(verification);
        artifact.producer = Some(producer);
        if let Some(destination_path) = artifact
            .plan
            .files()
            .iter()
            .find(|file| file.was_created())
            .map(|file| file.path().clone())
        {
            if self.inline_module_checks.len() >= MAX_INLINE_MODULE_CHECKS
                && let Some(oldest) = self.inline_module_checks.keys().next().cloned()
            {
                self.inline_module_checks.remove(&oldest);
            }
            let source_position = verified_position.unwrap_or(lsp_types::Position {
                line: 0,
                character: 0,
            });
            self.inline_module_checks.insert(
                artifact.plan.id().clone(),
                InlineModuleSemanticCheck {
                    source_path,
                    destination_path,
                    module_name: module_name.to_string(),
                    source_position,
                    pre_verification: verification,
                },
            );
        }
        Ok(artifact)
    }

    async fn native_inline_module_move_edit(
        &self,
        source_path: &Path,
        position: lsp_types::Position,
    ) -> Option<WorkspaceEdit> {
        let line = position.line.saturating_add(1);
        let character = position.character.saturating_add(1);
        let actions = self
            .translator
            .request_code_actions(
                source_path.display().to_string(),
                line,
                character,
                line,
                character,
                Some("refactor.extract".to_string()),
            )
            .await
            .ok()?;
        let mut action = take_code_action_by_assist_id(actions, "move_module_to_file")?;
        if action.disabled.is_some() || action.command.is_some() {
            return None;
        }
        if action.edit.is_none() {
            action = self
                .translator
                .resolve_code_action(&source_path.display().to_string(), action)
                .await
                .ok()?;
        }
        if action.disabled.is_some() || action.command.is_some() {
            return None;
        }
        action.edit
    }

    fn take_edit_plan(&mut self, plan_id: &PlanId, project_id: &str) -> Result<EditPlan, String> {
        self.edit_plans
            .take_for_project(plan_id, project_id)
            .map_err(|error| error.to_string())
    }

    fn configure_edit_safety(
        &mut self,
        boundary: &WorkspaceBoundary,
    ) -> Result<Option<BackupPolicy>, String> {
        let Some(safety) = self.edit_safety.as_ref() else {
            return Ok(None);
        };
        if let Some(audit) = &safety.audit_log {
            let path = resolve_edit_safety_path(boundary, &audit.path);
            boundary
                .validate_target(&path)
                .map_err(|error| format!("invalid audit log path {}: {error}", path.display()))?;
            let policy = AuditLogPolicy::new(&path, audit.max_bytes, audit.failure_mode)
                .map_err(|error| error.to_string())?;
            self.edit_plans.set_audit_log(policy);
        }
        safety
            .backup
            .as_ref()
            .map(|backup| {
                BackupPolicy::new(
                    boundary,
                    &backup.root,
                    backup.max_archives,
                    backup.max_bytes,
                    backup.failure_mode,
                )
                .map_err(|error| error.to_string())
            })
            .transpose()
    }

    fn record_edit_failure(&mut self, audit: EditAuditRecord, error: String) -> String {
        let _ = self
            .edit_plans
            .record_audit_with_policy(audit.failed(error.clone(), false));
        error
    }

    async fn verify_inline_module_after_apply(
        &mut self,
        check: &InlineModuleSemanticCheck,
    ) -> VerificationStatus {
        if check.pre_verification != VerificationStatus::SemanticVerified {
            return check.pre_verification;
        }
        let source_symbols = self
            .translator
            .handle_document_symbols(check.source_path.display().to_string())
            .await;
        let destination_symbols = self
            .translator
            .handle_document_symbols(check.destination_path.display().to_string())
            .await;
        let source_diagnostics = self
            .translator
            .handle_actor_diagnostics(check.source_path.display().to_string())
            .await;
        let destination_diagnostics = self
            .translator
            .handle_actor_diagnostics(check.destination_path.display().to_string())
            .await;
        let references = self
            .translator
            .handle_references(
                check.source_path.display().to_string(),
                check.source_position.line.saturating_add(1),
                check.source_position.character.saturating_add(1),
                true,
            )
            .await;
        let source_module_present = source_symbols.is_ok_and(|result| {
            result.symbols.iter().any(|symbol| {
                logical_module_name(&symbol.name) == logical_module_name(&check.module_name)
                    && symbol.kind.eq_ignore_ascii_case("Module")
            })
        });
        if source_module_present
            && destination_symbols.is_ok()
            && source_diagnostics.is_ok()
            && destination_diagnostics.is_ok_and(|result| diagnostics_are_error_free(&result))
            && references.is_ok()
        {
            VerificationStatus::SemanticVerified
        } else {
            VerificationStatus::SemanticPostcheckFailed
        }
    }

    async fn apply_edit_plan_with_context(
        &mut self,
        plan_id: &PlanId,
        project_id: &str,
        root: &Path,
        session_id: Option<String>,
        principal: Option<String>,
    ) -> Result<AppliedEditPlan, String> {
        let boundary = WorkspaceBoundary::new(root).map_err(|error| error.to_string())?;
        let backup_policy = self.configure_edit_safety(&boundary)?;
        let plan = self
            .edit_plans
            .take_for_project(plan_id, project_id)
            .map_err(|error| error.to_string())?;
        let semantic_check = self.inline_module_checks.remove(plan_id);
        let resource_operations = plan.file_operations().to_vec();
        let text_changes = planned_text_changes(&plan);
        let open_documents = plan
            .open_document_snapshots()
            .map(|snapshot| {
                (
                    snapshot.path().clone(),
                    snapshot.version().unwrap_or_default(),
                    snapshot.planned_content().to_string(),
                )
            })
            .collect::<Vec<_>>();
        let audit = EditAuditRecord::for_plan_with_context(&plan, session_id, principal);
        let apply_result = match backup_policy.as_ref() {
            Some(policy) => apply_plan_with_documents_and_backup(
                &boundary,
                &plan,
                self.translator.document_tracker(),
                policy,
            ),
            None => apply_plan_with_documents(&boundary, &plan, self.translator.document_tracker()),
        };
        let ApplyReport { committed_files } = match apply_result {
            Ok(report) => report,
            Err(error) => {
                return Err(self.record_edit_failure(audit, error.to_string()));
            }
        };
        let mut document_sync_failures = Vec::new();
        for (path, version, content) in open_documents {
            match self
                .translator
                .apply_open_document_content(&path, version, content)
                .await
            {
                Ok(failures) => document_sync_failures.extend(failures),
                Err(error) => return Err(self.record_edit_failure(audit, error.to_string())),
            }
        }
        let mut provider_synchronization = self
            .translator
            .synchronize_resource_operations(&resource_operations)
            .await;
        for result in self
            .translator
            .synchronize_text_changes(&text_changes)
            .await
        {
            merge_provider_synchronization(&mut provider_synchronization, result);
        }
        for (provider, error) in document_sync_failures {
            let message = format!("open-document synchronization failed: {error}");
            if let Some(result) = provider_synchronization
                .iter_mut()
                .find(|result| result.provider == provider.as_str())
            {
                result.synchronized = false;
                result.message = Some(result.message.take().map_or_else(
                    || message.clone(),
                    |existing| format!("{message}; {existing}"),
                ));
            } else {
                provider_synchronization.push(ProviderSynchronization {
                    provider: provider.to_string(),
                    synchronized: false,
                    watched_file_notifications: 0,
                    message: Some(message),
                });
            }
        }
        let verification = if let Some(check) = semantic_check.as_ref() {
            Some(self.verify_inline_module_after_apply(check).await)
        } else {
            None
        };
        self.edit_plans
            .record_audit_with_policy(audit.committed(committed_files.clone()))
            .map_err(|error| error.to_string())?;
        Ok(AppliedEditPlan {
            plan_id: plan.id().clone(),
            operations: plan.operations().to_vec(),
            unified_diff: plan.unified_diff().to_string(),
            committed_files,
            verification,
            provider_synchronization,
        })
    }

    async fn hover(
        &self,
        file_path: String,
        line: u32,
        character: u32,
    ) -> Result<HoverResult, String> {
        self.translator
            .handle_hover(file_path, line, character)
            .await
            .map_err(|error| error.to_string())
    }

    async fn definition(
        &self,
        file_path: String,
        line: u32,
        character: u32,
    ) -> Result<DefinitionResult, String> {
        self.translator
            .handle_definition(file_path, line, character)
            .await
            .map_err(|error| error.to_string())
    }

    async fn references(
        &self,
        file_path: String,
        line: u32,
        character: u32,
        include_declaration: bool,
    ) -> Result<ReferencesResult, String> {
        self.translator
            .handle_references(file_path, line, character, include_declaration)
            .await
            .map_err(|error| error.to_string())
    }

    async fn diagnostics(&mut self, file_path: String) -> Result<DiagnosticsResult, String> {
        self.translator
            .handle_actor_diagnostics(file_path)
            .await
            .map_err(|error| error.to_string())
    }

    async fn rename(
        &self,
        file_path: String,
        line: u32,
        character: u32,
        new_name: String,
    ) -> Result<RenameResult, String> {
        self.translator
            .handle_rename(file_path, line, character, new_name)
            .await
            .map_err(|error| error.to_string())
    }

    async fn rename_workspace_edit(
        &self,
        file_path: String,
        line: u32,
        character: u32,
        new_name: String,
    ) -> Result<Option<WorkspaceEdit>, String> {
        self.translator
            .request_rename_workspace_edit(file_path, line, character, new_name)
            .await
            .map_err(|error| error.to_string())
    }

    async fn completions(
        &self,
        file_path: String,
        line: u32,
        character: u32,
        trigger: Option<String>,
    ) -> Result<CompletionsResult, String> {
        self.translator
            .handle_completions(file_path, line, character, trigger)
            .await
            .map_err(|error| error.to_string())
    }

    async fn document_symbols(&self, file_path: String) -> Result<DocumentSymbolsResult, String> {
        self.translator
            .handle_document_symbols(file_path)
            .await
            .map_err(|error| error.to_string())
    }

    async fn format_document(
        &self,
        file_path: String,
        tab_size: u32,
        insert_spaces: bool,
    ) -> Result<FormatDocumentResult, String> {
        self.translator
            .handle_format_document(file_path, tab_size, insert_spaces)
            .await
            .map_err(|error| error.to_string())
    }

    async fn format_workspace_edit(
        &self,
        file_path: String,
        tab_size: u32,
        insert_spaces: bool,
    ) -> Result<Option<WorkspaceEdit>, String> {
        self.translator
            .request_format_workspace_edit(file_path, tab_size, insert_spaces)
            .await
            .map_err(|error| error.to_string())
    }

    async fn range_format_workspace_edit(
        &self,
        file_path: String,
        start: (u32, u32),
        end: (u32, u32),
        tab_size: u32,
        insert_spaces: bool,
    ) -> Result<SupportedWorkspaceEdit, String> {
        self.translator
            .request_range_format_workspace_edit(file_path, start, end, tab_size, insert_spaces)
            .await
            .map_err(|error| error.to_string())
    }

    async fn move_item_workspace_edit(
        &self,
        file_path: String,
        start: (u32, u32),
        end: (u32, u32),
        direction: &str,
    ) -> Result<SupportedWorkspaceEdit, String> {
        self.translator
            .request_move_item_workspace_edit(file_path, start, end, direction)
            .await
            .map_err(|error| error.to_string())
    }

    async fn semantic_discovery(
        &self,
        file_path: String,
        line: u32,
        character: u32,
        kind: SemanticDiscoveryKind,
    ) -> Result<SemanticDiscoveryResult, String> {
        self.translator
            .request_semantic_discovery(file_path, line, character, kind)
            .await
            .map_err(|error| error.to_string())
    }

    async fn workspace_symbol(
        &self,
        query: String,
        kind_filter: Option<String>,
        limit: u32,
    ) -> Result<WorkspaceSymbolResult, String> {
        self.translator
            .handle_workspace_symbol(query, kind_filter, limit)
            .await
            .map_err(|error| error.to_string())
    }

    async fn code_actions(
        &self,
        file_path: String,
        start_line: u32,
        start_character: u32,
        end_line: u32,
        end_character: u32,
        kind_filter: Option<String>,
    ) -> Result<CodeActionsResult, String> {
        self.translator
            .handle_code_actions(
                file_path,
                start_line,
                start_character,
                end_line,
                end_character,
                kind_filter,
            )
            .await
            .map_err(|error| error.to_string())
    }

    async fn code_action_list(
        &mut self,
        file_path: String,
        start_line: u32,
        start_character: u32,
        end_line: u32,
        end_character: u32,
        kind_filter: Option<String>,
    ) -> Result<CodeActionsResult, String> {
        let actions = self
            .translator
            .request_code_actions(
                file_path.clone(),
                start_line,
                start_character,
                end_line,
                end_character,
                kind_filter,
            )
            .await
            .map_err(|error| error.to_string())?;
        let mut result = Vec::with_capacity(actions.len());
        for action in actions {
            let id = self.code_actions.insert(StoredCodeAction {
                file_path: file_path.clone(),
                action: action.clone(),
                created_at: Instant::now(),
            });
            result.push(convert_code_action_or_command(action, Some(id.to_string())));
        }
        Ok(CodeActionsResult { actions: result })
    }

    async fn preview_code_action(
        &mut self,
        action_id: PlanId,
        project_id: &str,
        encoding: PositionEncoding,
        root: &Path,
    ) -> Result<PreviewArtifact, String> {
        let stored = self.code_actions.take(&action_id)?;
        let action = match stored.action {
            lsp_types::CodeActionOrCommand::Command(_) => {
                return Err("command-only code actions are unsupported".to_string());
            }
            lsp_types::CodeActionOrCommand::CodeAction(mut action) => {
                if let Some(reason) = action
                    .disabled
                    .as_ref()
                    .map(|disabled| disabled.reason.clone())
                {
                    return Err(format!("code action is disabled: {reason}"));
                }
                if action.command.is_some() {
                    return Err("code actions with commands are unsupported".to_string());
                }
                if action.edit.is_none() {
                    if action.data.is_none() {
                        return Err("code action has no workspace edit".to_string());
                    }
                    action = self
                        .translator
                        .resolve_code_action(&stored.file_path, action)
                        .await
                        .map_err(|error| error.to_string())?;
                }
                if let Some(reason) = action
                    .disabled
                    .as_ref()
                    .map(|disabled| disabled.reason.clone())
                {
                    return Err(format!("resolved code action is disabled: {reason}"));
                }
                if action.command.is_some() {
                    return Err("resolved code actions with commands are unsupported".to_string());
                }
                action
            }
        };
        let edit = action
            .edit
            .ok_or_else(|| "resolved code action has no workspace edit".to_string())?;
        self.preview_edit(project_id, edit, encoding, root)
    }

    async fn prepare_call_hierarchy(
        &self,
        file_path: String,
        line: u32,
        character: u32,
    ) -> Result<CallHierarchyPrepareResult, String> {
        self.translator
            .handle_call_hierarchy_prepare(file_path, line, character)
            .await
            .map_err(|error| error.to_string())
    }

    async fn incoming_calls(&self, item: serde_json::Value) -> Result<IncomingCallsResult, String> {
        self.translator
            .handle_incoming_calls(item)
            .await
            .map_err(|error| error.to_string())
    }

    async fn outgoing_calls(&self, item: serde_json::Value) -> Result<OutgoingCallsResult, String> {
        self.translator
            .handle_outgoing_calls(item)
            .await
            .map_err(|error| error.to_string())
    }

    async fn signature_help(
        &self,
        file_path: String,
        line: u32,
        character: u32,
    ) -> Result<SignatureHelpResult, String> {
        self.translator
            .handle_signature_help(file_path, line, character)
            .await
            .map_err(|error| error.to_string())
    }

    async fn inlay_hints(
        &self,
        file_path: String,
        start_line: u32,
        start_character: u32,
        end_line: u32,
        end_character: u32,
    ) -> Result<InlayHintsResult, String> {
        self.translator
            .handle_inlay_hints(
                file_path,
                start_line,
                start_character,
                end_line,
                end_character,
            )
            .await
            .map_err(|error| error.to_string())
    }

    async fn go_to_implementation(
        &self,
        file_path: String,
        line: u32,
        character: u32,
    ) -> Result<LocationsResult, String> {
        self.translator
            .handle_implementation(file_path, line, character)
            .await
            .map_err(|error| error.to_string())
    }

    async fn go_to_type_definition(
        &self,
        file_path: String,
        line: u32,
        character: u32,
    ) -> Result<LocationsResult, String> {
        self.translator
            .handle_type_definition(file_path, line, character)
            .await
            .map_err(|error| error.to_string())
    }

    fn cached_diagnostics(&self, file_path: &str) -> Result<DiagnosticsResult, String> {
        self.translator
            .handle_cached_diagnostics(file_path)
            .map_err(|error| error.to_string())
    }

    fn has_cached_diagnostics(&self, file_path: &str) -> Result<bool, String> {
        self.translator
            .has_cached_diagnostics(file_path)
            .map_err(|error| error.to_string())
    }

    fn validate_path(&self, file_path: &str) -> Result<(), String> {
        self.translator
            .validate_path(Path::new(file_path))
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    fn server_logs(
        &self,
        limit: usize,
        min_level: Option<String>,
    ) -> Result<ServerLogsResult, String> {
        self.translator
            .actor_server_logs(limit, min_level)
            .map_err(|error| error.to_string())
    }

    fn server_messages(&self, limit: usize) -> Result<ServerMessagesResult, String> {
        self.translator
            .actor_server_messages(limit)
            .map_err(|error| error.to_string())
    }

    fn server_capabilities(
        &self,
        language_id: Option<&str>,
    ) -> Result<Vec<ServerCapability>, String> {
        self.translator
            .server_capabilities(language_id)
            .map_err(|error| error.to_string())
    }

    fn notification(
        &mut self,
        generation: u64,
        server_id: &ServerId,
        notification: LspNotification,
    ) -> Option<ProjectEvent> {
        let completes_initial_load = notification.completes_initial_load();
        match notification {
            LspNotification::PublishDiagnostics(params) => {
                let event = ProjectEvent::DiagnosticsUpdated {
                    uri: params.uri.to_string(),
                    version: params.version,
                    diagnostic_count: params.diagnostics.len(),
                };
                self.translator.notification_cache_mut().store_diagnostics(
                    server_id,
                    &params.uri,
                    params.version,
                    params.diagnostics,
                );
                Some(event)
            }
            LspNotification::LogMessage(params) => {
                self.translator
                    .notification_cache_mut()
                    .store_log_with_generation(generation, params.typ.into(), params.message);
                None
            }
            LspNotification::ShowMessage(params) => {
                self.translator
                    .notification_cache_mut()
                    .store_message_with_generation(generation, params.typ.into(), params.message);
                None
            }
            LspNotification::ServerStatus(_) | LspNotification::Progress { .. } => {
                if completes_initial_load {
                    self.translator.clear_expected_server(server_id);
                }
                None
            }
            LspNotification::Other { .. } => None,
        }
    }

    async fn shutdown(&mut self) -> Result<(), String> {
        self.translator
            .shutdown()
            .await
            .map_err(|error| error.to_string())
    }

    async fn activate_workspace_roots(
        &mut self,
        roots: Vec<PathBuf>,
    ) -> Result<ProjectActivation, String> {
        self.translator
            .activate_project_with_roots(roots)
            .await
            .map_err(|error| error.to_string())
    }

    async fn add_workspace_root(&mut self, root: PathBuf) -> Result<ProjectActivation, String> {
        self.translator
            .add_workspace_root(root)
            .await
            .map_err(|error| error.to_string())
    }

    async fn restart(&mut self) -> Result<ProjectActivation, String> {
        let roots = self.translator.workspace_roots().to_vec();
        if roots.is_empty() {
            return Ok(ProjectActivation::ready());
        }
        if self.translator.configured_language_ids().is_empty() {
            return Ok(ProjectActivation::ready());
        }
        self.shutdown().await?;
        self.translator
            .activate_project_with_roots(roots)
            .await
            .map_err(|error| error.to_string())
    }

    fn summary(&self) -> ProjectRuntimeSummary {
        ProjectRuntimeSummary::from_translator(&self.translator, self.generation)
    }

    fn open_document_paths(&self) -> Vec<PathBuf> {
        self.translator.document_tracker().open_paths()
    }

    fn has_dirty_documents(&self) -> bool {
        self.translator.document_tracker().has_dirty_documents()
    }
}

fn code_action_has_assist_id(action: &lsp_types::CodeAction, expected: &str) -> bool {
    action
        .data
        .as_ref()
        .and_then(|data| data.get("id"))
        .and_then(serde_json::Value::as_str)
        .and_then(|id| id.strip_prefix(expected))
        .is_some_and(|suffix| suffix.is_empty() || suffix.starts_with(':'))
}

fn take_code_action_by_assist_id(
    actions: Vec<lsp_types::CodeActionOrCommand>,
    expected: &str,
) -> Option<lsp_types::CodeAction> {
    actions.into_iter().find_map(|action| match action {
        lsp_types::CodeActionOrCommand::CodeAction(action)
            if code_action_has_assist_id(&action, expected) =>
        {
            Some(action)
        }
        _ => None,
    })
}

fn diagnostics_are_error_free(result: &DiagnosticsResult) -> bool {
    result
        .diagnostics
        .iter()
        .all(|diagnostic| !matches!(diagnostic.severity, DiagnosticSeverity::Error))
}

async fn recover_project_after_server_exit(
    actor_sender: &mpsc::WeakSender<ProjectRequest>,
    channels: &ProjectActorChannels,
    state: &mut ProjectState,
    runtime: &mut ProjectRuntime,
    residency: Option<&ProjectResidency>,
) {
    loop {
        let Some(attempt) = runtime.begin_automatic_restart() else {
            channels.publish_failure(state, LANGUAGE_SERVER_EXITED);
            return;
        };

        state.last_error = Some(format!(
            "{LANGUAGE_SERVER_EXITED}; restarting (attempt {}/{MAX_AUTOMATIC_RESTART_ATTEMPTS})",
            attempt.number
        ));
        channels.publish_status(state, ProjectStatus::Restarting);
        tokio::time::sleep(attempt.delay).await;
        match runtime.restart().await {
            Ok(notification_receivers) => {
                state.last_error = None;
                mark_project_started(
                    notification_receivers,
                    actor_sender,
                    channels,
                    state,
                    runtime,
                    residency,
                );
                return;
            }
            Err(error) if attempt.number < MAX_AUTOMATIC_RESTART_ATTEMPTS => {
                state.sync_runtime(runtime);
                state.last_error = Some(format!(
                    "automatic restart attempt {} failed: {error}",
                    attempt.number
                ));
            }
            Err(error) => {
                state.sync_runtime(runtime);
                channels.publish_failure(state, error);
                return;
            }
        }
    }
}

async fn handle_server_exit(
    generation: u64,
    actor_sender: &mpsc::WeakSender<ProjectRequest>,
    channels: &ProjectActorChannels,
    state: &mut ProjectState,
    runtime: &mut ProjectRuntime,
    residency: Option<&ProjectResidency>,
) {
    if !runtime.owns_generation(generation) {
        return;
    }

    channels.publish(ProjectEvent::ServerExited { generation });
    match state.status {
        ProjectStatus::Ready | ProjectStatus::Degraded => {
            recover_project_after_server_exit(actor_sender, channels, state, runtime, residency)
                .await;
        }
        ProjectStatus::Starting | ProjectStatus::Restarting => {
            channels.publish_failure(state, LANGUAGE_SERVER_EXITED);
        }
        ProjectStatus::Failed
        | ProjectStatus::Stopping
        | ProjectStatus::Dormant
        | ProjectStatus::Stopped => {}
    }
}

/// Spawn a bounded project actor with `Starting` as its initial status.
#[must_use]
pub fn spawn_project_actor(capacity: usize) -> ProjectHandle {
    spawn_project_actor_with_translator(capacity, Translator::new())
}

/// Spawn an actor whose translator is configured for one canonical project root.
#[must_use]
pub fn spawn_project_actor_for_root(capacity: usize, root: &CanonicalRoot) -> ProjectHandle {
    let mut translator = Translator::new();
    translator.set_workspace_roots(vec![root.as_path().to_path_buf()]);
    spawn_project_actor_with_translator(capacity, translator)
}

/// Spawn an actor using a configuration snapshot from the daemon translator.
#[must_use]
pub fn spawn_project_actor_for_root_with_template(
    capacity: usize,
    root: &CanonicalRoot,
    template: &TranslatorTemplate,
) -> ProjectHandle {
    spawn_project_actor_with_translator_and_safety(
        capacity,
        template.translator_for_root(root.as_path().to_path_buf()),
        template.edit_safety().cloned(),
    )
}

/// Spawn an actor with translator state owned exclusively by that actor.
#[must_use]
pub fn spawn_project_actor_with_translator(
    capacity: usize,
    translator: Translator,
) -> ProjectHandle {
    spawn_project_actor_with_translator_and_safety(capacity, translator, None)
}

fn spawn_project_actor_with_translator_and_safety(
    capacity: usize,
    translator: Translator,
    edit_safety: Option<EditSafetyConfig>,
) -> ProjectHandle {
    spawn_project_actor_with_runtime(capacity, translator, edit_safety, None)
}

#[derive(Clone)]
struct ProjectResidency {
    controller: RustResidencyController,
    group: RustGroupId,
}

impl ProjectResidency {
    async fn resident_request(&self, request: ProjectRequest) -> ProjectRequest {
        let guard = self.controller.acquire(self.group).await;
        ProjectRequest::Resident {
            request: Box::new(request),
            guard,
        }
    }
}

fn spawn_project_actor_with_runtime(
    capacity: usize,
    translator: Translator,
    edit_safety: Option<EditSafetyConfig>,
    residency: Option<ProjectResidency>,
) -> ProjectHandle {
    let (sender, receiver) = mpsc::channel(capacity.max(1));
    let actor_sender = sender.downgrade();
    if let Some(residency) = &residency {
        residency
            .controller
            .register(residency.group, actor_sender.clone());
    }
    let sender = residency.as_ref().map_or_else(
        || ProjectRequestSender::new(sender.clone()),
        |residency| ProjectRequestSender::with_residency(sender.clone(), residency.clone()),
    );
    let (status_tx, status_rx) = watch::channel(ProjectStatus::Starting);
    let (event_tx, _) = broadcast::channel(256);
    let event_sender = event_tx.clone();
    let event_history = std::sync::Arc::new(std::sync::Mutex::new(ProjectEventHistory::new(256)));
    let channels = ProjectActorChannels {
        status_tx,
        event_tx,
        event_history: std::sync::Arc::clone(&event_history),
    };
    let runtime = match edit_safety {
        None => ProjectRuntime::new(translator),
        Some(safety) => ProjectRuntime::with_edit_safety(translator, Some(safety)),
    };
    tokio::spawn(run_project_actor(
        receiver,
        actor_sender,
        channels,
        ProjectState::new(ProjectStatus::Starting, runtime.summary()),
        runtime,
        residency,
    ));
    ProjectHandle {
        sender,
        status: status_rx,
        events: event_sender,
        event_history,
    }
}

struct ProjectActorChannels {
    status_tx: watch::Sender<ProjectStatus>,
    event_tx: broadcast::Sender<ProjectEvent>,
    event_history: std::sync::Arc<std::sync::Mutex<ProjectEventHistory>>,
}

impl ProjectActorChannels {
    fn publish(&self, event: ProjectEvent) {
        self.event_history
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .record(event.clone());
        let _ = self.event_tx.send(event);
    }

    fn publish_notification(
        &self,
        runtime: &mut ProjectRuntime,
        generation: u64,
        server_id: &ServerId,
        notification: LspNotification,
    ) {
        if !runtime.owns_generation(generation) {
            return;
        }
        if let Some(event) = runtime.notification(generation, server_id, notification) {
            self.publish(event);
        }
    }

    fn publish_applied_edit(&self, applied: &AppliedEditPlan) {
        for event in applied.project_events() {
            self.publish(event);
        }
    }

    fn publish_status(&self, state: &mut ProjectState, status: ProjectStatus) {
        state.status = status;
        let _ = self.status_tx.send(status);
        self.publish(ProjectEvent::StatusChanged {
            status,
            last_error: state.last_error.clone(),
        });
    }

    fn publish_failure(&self, state: &mut ProjectState, error: impl Into<String>) {
        state.last_error = Some(error.into());
        self.publish_status(state, ProjectStatus::Failed);
    }
}

async fn run_project_actor(
    mut receiver: mpsc::Receiver<ProjectRequest>,
    actor_sender: mpsc::WeakSender<ProjectRequest>,
    channels: ProjectActorChannels,
    mut state: ProjectState,
    mut runtime: ProjectRuntime,
    residency: Option<ProjectResidency>,
) {
    while let Some(request) = next_project_request(&mut receiver).await {
        let (request, _residency_guard) = request.into_resident();
        let resumes_runtime = request.resumes_rust_runtime();
        if residency.is_some()
            && resumes_runtime
            && !runtime.has_active_workspace_roots(runtime.translator.workspace_roots())
        {
            resume_project_runtime(
                &actor_sender,
                &channels,
                &mut state,
                &mut runtime,
                residency.as_ref(),
            )
            .await;
        }
        if handle_project_request(
            request,
            &actor_sender,
            &channels,
            &mut state,
            &mut runtime,
            residency.as_ref(),
        )
        .await
        {
            break;
        }
    }
    if state.status != ProjectStatus::Stopped {
        stop_project_runtime(&channels, &mut state, &mut runtime, false).await;
    }
    if let Some(residency) = residency {
        residency.controller.remove(residency.group);
    }
}

async fn resume_project_runtime(
    actor_sender: &mpsc::WeakSender<ProjectRequest>,
    channels: &ProjectActorChannels,
    state: &mut ProjectState,
    runtime: &mut ProjectRuntime,
    residency: Option<&ProjectResidency>,
) {
    let roots = runtime.translator.workspace_roots().to_vec();
    runtime.begin_transition();
    state.last_error = None;
    channels.publish_status(state, ProjectStatus::Starting);
    match runtime.activate_workspace_roots(roots).await {
        Ok(activation) => {
            mark_project_started(
                activation,
                actor_sender,
                channels,
                state,
                runtime,
                residency,
            );
        }
        Err(error) => {
            state.sync_runtime(runtime);
            channels.publish_failure(state, error);
        }
    }
}

async fn next_project_request(
    receiver: &mut mpsc::Receiver<ProjectRequest>,
) -> Option<ProjectRequest> {
    while let Some(request) = receiver.recv().await {
        if !request.is_cancelled() {
            return Some(request);
        }
    }
    None
}

fn spawn_notification_forwarders(
    notification_receivers: Vec<(ServerId, mpsc::Receiver<LspNotification>)>,
    actor_sender: &mpsc::WeakSender<ProjectRequest>,
    generation: u64,
    residency: Option<&ProjectResidency>,
) {
    for (server_id, receiver) in notification_receivers {
        let sender = actor_sender.clone();
        tokio::spawn(forward_lsp_notifications(
            server_id,
            receiver,
            sender,
            generation,
            residency.cloned(),
        ));
    }
}

async fn forward_lsp_notifications(
    server_id: ServerId,
    mut receiver: mpsc::Receiver<LspNotification>,
    sender: mpsc::WeakSender<ProjectRequest>,
    generation: u64,
    residency: Option<ProjectResidency>,
) {
    while let Some(notification) = receiver.recv().await {
        let Some(sender) = sender.upgrade() else {
            break;
        };
        if sender
            .send(ProjectRequest::Notification {
                generation,
                server_id: server_id.clone(),
                notification,
            })
            .await
            .is_err()
        {
            break;
        }
    }
    if let Some(sender) = sender.upgrade() {
        let request = ProjectRequest::ServerExited { generation };
        let request = if let Some(residency) = residency {
            residency.resident_request(request).await
        } else {
            request
        };
        let _ = sender.send(request).await;
    }
}

fn mark_project_started(
    activation: ProjectActivation,
    actor_sender: &mpsc::WeakSender<ProjectRequest>,
    channels: &ProjectActorChannels,
    state: &mut ProjectState,
    runtime: &mut ProjectRuntime,
    residency: Option<&ProjectResidency>,
) {
    runtime.reset_automatic_restart();
    let health = activation.health();
    spawn_notification_forwarders(
        activation.into_notification_receivers(),
        actor_sender,
        runtime.generation(),
        residency,
    );
    publish_project_readiness(channels, state, runtime, health);
}

const fn activation_status(health: ActivationHealth, initializing: bool) -> ProjectStatus {
    if initializing {
        ProjectStatus::Starting
    } else {
        match health {
            ActivationHealth::Ready => ProjectStatus::Ready,
            ActivationHealth::Degraded => ProjectStatus::Degraded,
        }
    }
}

fn publish_project_readiness(
    channels: &ProjectActorChannels,
    state: &mut ProjectState,
    runtime: &ProjectRuntime,
    health: ActivationHealth,
) {
    state.sync_runtime(runtime);
    channels.publish_status(
        state,
        activation_status(health, runtime.translator.is_initializing()),
    );
}

async fn stop_project_runtime(
    channels: &ProjectActorChannels,
    state: &mut ProjectState,
    runtime: &mut ProjectRuntime,
    clear_error: bool,
) {
    runtime.begin_transition();
    if clear_error {
        state.last_error = None;
    }
    channels.publish_status(state, ProjectStatus::Stopping);
    if let Err(error) = runtime.shutdown().await {
        state.last_error = Some(error);
    }
    state.sync_runtime(runtime);
    channels.publish_status(state, ProjectStatus::Stopped);
}

async fn suspend_project_runtime(
    channels: &ProjectActorChannels,
    state: &mut ProjectState,
    runtime: &mut ProjectRuntime,
) -> Result<(), ()> {
    if runtime.has_dirty_documents() {
        return Err(());
    }
    runtime.begin_transition();
    state.last_error = None;
    channels.publish_status(state, ProjectStatus::Stopping);
    if let Err(error) = runtime.shutdown().await {
        state.last_error = Some(error);
        state.sync_runtime(runtime);
        channels.publish_status(state, ProjectStatus::Failed);
        return Err(());
    }
    state.sync_runtime(runtime);
    channels.publish_status(state, ProjectStatus::Dormant);
    Ok(())
}

// This exhaustive dispatcher keeps actor state transitions in one place; each
// request arm is intentionally small and independently typed.
#[allow(clippy::too_many_lines)]
#[allow(clippy::large_stack_frames)]
async fn handle_project_request(
    request: ProjectRequest,
    actor_sender: &mpsc::WeakSender<ProjectRequest>,
    channels: &ProjectActorChannels,
    state: &mut ProjectState,
    runtime: &mut ProjectRuntime,
    residency: Option<&ProjectResidency>,
) -> bool {
    let Ok(request) = request.reject_if_failed(state.status) else {
        return false;
    };

    match request {
        ProjectRequest::Resident { .. } => {
            unreachable!("resident request must be unwrapped by the actor loop")
        }
        ProjectRequest::Query { reply } | ProjectRequest::Refresh { reply } => {
            state.sync_runtime(runtime);
            let _ = reply.send(state.clone());
        }
        ProjectRequest::Suspend { reply } => {
            let _ = reply.send(suspend_project_runtime(channels, state, runtime).await);
        }
        ProjectRequest::Activate { root, reply } => {
            if runtime.activation_is_reusable(state.status, std::slice::from_ref(&root)) {
                state.sync_runtime(runtime);
                let _ = reply.send(Ok(state.clone()));
                return false;
            }
            runtime.begin_transition();
            state.last_error = None;
            channels.publish_status(state, ProjectStatus::Starting);
            match runtime.translator.activate_project(root).await {
                Ok(notification_receivers) => {
                    mark_project_started(
                        notification_receivers,
                        actor_sender,
                        channels,
                        state,
                        runtime,
                        residency,
                    );
                    let _ = reply.send(Ok(state.clone()));
                }
                Err(error) => {
                    state.sync_runtime(runtime);
                    channels.publish_failure(state, error.to_string());
                    let _ = reply.send(Err(error.to_string()));
                }
            }
        }
        ProjectRequest::ActivateWorkspaceRoots { roots, reply } => {
            if runtime.activation_is_reusable(state.status, &roots) {
                state.sync_runtime(runtime);
                let _ = reply.send(Ok(state.clone()));
                return false;
            }
            runtime.begin_transition();
            state.last_error = None;
            channels.publish_status(state, ProjectStatus::Starting);
            match runtime.activate_workspace_roots(roots).await {
                Ok(notification_receivers) => {
                    mark_project_started(
                        notification_receivers,
                        actor_sender,
                        channels,
                        state,
                        runtime,
                        residency,
                    );
                    let _ = reply.send(Ok(state.clone()));
                }
                Err(error) => {
                    state.sync_runtime(runtime);
                    channels.publish_failure(state, error.clone());
                    let _ = reply.send(Err(error));
                }
            }
        }
        ProjectRequest::Hover {
            file_path,
            line,
            character,
            reply,
        } => {
            let _ = reply.send(runtime.hover(file_path, line, character).await);
        }
        ProjectRequest::Definition {
            file_path,
            line,
            character,
            reply,
        } => {
            let _ = reply.send(runtime.definition(file_path, line, character).await);
        }
        ProjectRequest::References {
            file_path,
            line,
            character,
            include_declaration,
            reply,
        } => {
            let _ = reply.send(
                runtime
                    .references(file_path, line, character, include_declaration)
                    .await,
            );
        }
        ProjectRequest::Diagnostics { file_path, reply } => {
            let _ = reply.send(runtime.diagnostics(file_path).await);
        }
        ProjectRequest::Rename {
            file_path,
            line,
            character,
            new_name,
            reply,
        } => {
            let _ = reply.send(runtime.rename(file_path, line, character, new_name).await);
        }
        ProjectRequest::RenameWorkspaceEdit {
            file_path,
            line,
            character,
            new_name,
            reply,
        } => {
            let _ = reply.send(
                runtime
                    .rename_workspace_edit(file_path, line, character, new_name)
                    .await,
            );
        }
        ProjectRequest::Completions {
            file_path,
            line,
            character,
            trigger,
            reply,
        } => {
            let _ = reply.send(
                runtime
                    .completions(file_path, line, character, trigger)
                    .await,
            );
        }
        ProjectRequest::DocumentSymbols { file_path, reply } => {
            let _ = reply.send(runtime.document_symbols(file_path).await);
        }
        ProjectRequest::FormatDocument {
            file_path,
            tab_size,
            insert_spaces,
            reply,
        } => {
            let _ = reply.send(
                runtime
                    .format_document(file_path, tab_size, insert_spaces)
                    .await,
            );
        }
        ProjectRequest::FormatWorkspaceEdit {
            file_path,
            tab_size,
            insert_spaces,
            reply,
        } => {
            let _ = reply.send(
                runtime
                    .format_workspace_edit(file_path, tab_size, insert_spaces)
                    .await,
            );
        }
        ProjectRequest::RangeFormatWorkspaceEdit {
            file_path,
            start,
            end,
            tab_size,
            insert_spaces,
            reply,
        } => {
            let _ = reply.send(
                runtime
                    .range_format_workspace_edit(file_path, start, end, tab_size, insert_spaces)
                    .await,
            );
        }
        ProjectRequest::MoveItemWorkspaceEdit {
            file_path,
            start,
            end,
            direction,
            reply,
        } => {
            let _ = reply.send(
                runtime
                    .move_item_workspace_edit(file_path, start, end, &direction)
                    .await,
            );
        }
        ProjectRequest::SemanticDiscovery {
            file_path,
            line,
            character,
            kind,
            reply,
        } => {
            let _ = reply.send(
                runtime
                    .semantic_discovery(file_path, line, character, kind)
                    .await,
            );
        }
        ProjectRequest::WorkspaceSymbol {
            query,
            kind_filter,
            limit,
            reply,
        } => {
            let _ = reply.send(runtime.workspace_symbol(query, kind_filter, limit).await);
        }
        ProjectRequest::CodeActions {
            file_path,
            start_line,
            start_character,
            end_line,
            end_character,
            kind_filter,
            reply,
        } => {
            let _ = reply.send(
                runtime
                    .code_actions(
                        file_path,
                        start_line,
                        start_character,
                        end_line,
                        end_character,
                        kind_filter,
                    )
                    .await,
            );
        }
        ProjectRequest::CodeActionList {
            file_path,
            start_line,
            start_character,
            end_line,
            end_character,
            kind_filter,
            reply,
        } => {
            let _ = reply.send(
                runtime
                    .code_action_list(
                        file_path,
                        start_line,
                        start_character,
                        end_line,
                        end_character,
                        kind_filter,
                    )
                    .await,
            );
        }
        ProjectRequest::CodeActionPreview {
            action_id,
            project_id,
            encoding,
            root,
            reply,
        } => {
            let _ = reply.send(
                runtime
                    .preview_code_action(action_id, &project_id, encoding, &root)
                    .await,
            );
        }
        ProjectRequest::PrepareCallHierarchy {
            file_path,
            line,
            character,
            reply,
        } => {
            let _ = reply.send(
                runtime
                    .prepare_call_hierarchy(file_path, line, character)
                    .await,
            );
        }
        ProjectRequest::IncomingCalls { item, reply } => {
            let _ = reply.send(runtime.incoming_calls(item).await);
        }
        ProjectRequest::OutgoingCalls { item, reply } => {
            let _ = reply.send(runtime.outgoing_calls(item).await);
        }
        ProjectRequest::SignatureHelp {
            file_path,
            line,
            character,
            reply,
        } => {
            let _ = reply.send(runtime.signature_help(file_path, line, character).await);
        }
        ProjectRequest::InlayHints {
            file_path,
            start_line,
            start_character,
            end_line,
            end_character,
            reply,
        } => {
            let _ = reply.send(
                runtime
                    .inlay_hints(
                        file_path,
                        start_line,
                        start_character,
                        end_line,
                        end_character,
                    )
                    .await,
            );
        }
        ProjectRequest::GoToImplementation {
            file_path,
            line,
            character,
            reply,
        } => {
            let _ = reply.send(
                runtime
                    .go_to_implementation(file_path, line, character)
                    .await,
            );
        }
        ProjectRequest::GoToTypeDefinition {
            file_path,
            line,
            character,
            reply,
        } => {
            let _ = reply.send(
                runtime
                    .go_to_type_definition(file_path, line, character)
                    .await,
            );
        }
        ProjectRequest::CachedDiagnostics { file_path, reply } => {
            let _ = reply.send(runtime.cached_diagnostics(&file_path));
        }
        ProjectRequest::HasCachedDiagnostics { file_path, reply } => {
            let _ = reply.send(runtime.has_cached_diagnostics(&file_path));
        }
        ProjectRequest::OpenDocumentPaths { reply } => {
            let _ = reply.send(runtime.open_document_paths());
        }
        ProjectRequest::ValidatePath { file_path, reply } => {
            let _ = reply.send(runtime.validate_path(&file_path));
        }
        ProjectRequest::AddWorkspaceRoot { root, reply } => {
            runtime.begin_transition();
            state.last_error = None;
            channels.publish_status(state, ProjectStatus::Restarting);
            match runtime.add_workspace_root(root).await {
                Ok(notification_receivers) => {
                    mark_project_started(
                        notification_receivers,
                        actor_sender,
                        channels,
                        state,
                        runtime,
                        residency,
                    );
                    let _ = reply.send(Ok(state.clone()));
                }
                Err(error) => {
                    state.sync_runtime(runtime);
                    channels.publish_failure(state, error.clone());
                    let _ = reply.send(Err(error));
                }
            }
        }
        ProjectRequest::StoreEditPlan { plan, reply } => {
            let _ = reply.send(runtime.store_edit_plan(plan));
        }
        ProjectRequest::PreviewEdit {
            project_id,
            edit,
            encoding,
            root,
            reply,
        } => {
            let _ = reply.send(runtime.preview_edit(&project_id, edit, encoding, &root));
        }
        ProjectRequest::MoveInlineModulePreview {
            project_id,
            file_path,
            module_name,
            module_position,
            encoding,
            root,
            reply,
        } => {
            let _ = reply.send(
                runtime
                    .move_inline_module_preview(
                        &project_id,
                        &file_path,
                        &module_name,
                        module_position,
                        encoding,
                        &root,
                    )
                    .await,
            );
        }
        ProjectRequest::StructuralReplacePreview {
            project_id,
            request,
            root,
            mut reply,
        } => {
            let operation = runtime.structural_replace_preview(&project_id, request, &root);
            tokio::pin!(operation);
            tokio::select! {
                result = &mut operation => {
                    let _ = reply.send(result);
                }
                () = reply.closed() => {}
            }
        }
        ProjectRequest::PathRenamePreview {
            project_id,
            request,
            root,
            mut reply,
        } => {
            let operation = runtime.path_rename_preview(&project_id, request, &root);
            tokio::pin!(operation);
            tokio::select! {
                result = &mut operation => {
                    let _ = reply.send(result);
                }
                () = reply.closed() => {}
            }
        }
        ProjectRequest::TakeEditPlan {
            plan_id,
            project_id,
            reply,
        } => {
            let _ = reply.send(runtime.take_edit_plan(&plan_id, &project_id));
        }
        ProjectRequest::ApplyEditPlan {
            plan_id,
            project_id,
            root,
            session_id,
            principal,
            reply,
        } => {
            let result = runtime
                .apply_edit_plan_with_context(&plan_id, &project_id, &root, session_id, principal)
                .await;
            if let Ok(applied) = &result {
                channels.publish_applied_edit(applied);
            }
            let _ = reply.send(result);
        }
        ProjectRequest::PublishEvent { event, reply } => {
            channels.publish(event);
            let _ = reply.send(());
        }
        ProjectRequest::ServerLogs {
            limit,
            min_level,
            reply,
        } => {
            let _ = reply.send(runtime.server_logs(limit, min_level));
        }
        ProjectRequest::ServerMessages { limit, reply } => {
            let _ = reply.send(runtime.server_messages(limit));
        }
        ProjectRequest::ServerCapabilities { language_id, reply } => {
            let _ = reply.send(runtime.server_capabilities(language_id.as_deref()));
        }
        ProjectRequest::Notification {
            generation,
            server_id,
            notification,
        } => {
            let was_initializing = runtime.translator.is_initializing();
            channels.publish_notification(runtime, generation, &server_id, notification);
            if was_initializing && !runtime.translator.is_initializing() {
                let health = if state.status == ProjectStatus::Degraded {
                    ActivationHealth::Degraded
                } else {
                    ActivationHealth::Ready
                };
                publish_project_readiness(channels, state, runtime, health);
            }
        }
        ProjectRequest::ServerExited { generation } => {
            handle_server_exit(
                generation,
                actor_sender,
                channels,
                state,
                runtime,
                residency,
            )
            .await;
        }
        ProjectRequest::SetStatus { status, reply } => {
            state.sync_runtime(runtime);
            state.last_error = None;
            channels.publish_status(state, status);
            let _ = reply.send(());
        }
        ProjectRequest::Restart { reply } => {
            runtime.begin_transition();
            state.sync_runtime(runtime);
            state.last_error = None;
            channels.publish_status(state, ProjectStatus::Restarting);
            match runtime.restart().await {
                Ok(notification_receivers) => {
                    mark_project_started(
                        notification_receivers,
                        actor_sender,
                        channels,
                        state,
                        runtime,
                        residency,
                    );
                    let _ = reply.send(state.clone());
                }
                Err(error) => {
                    state.sync_runtime(runtime);
                    channels.publish_failure(state, error);
                    let _ = reply.send(state.clone());
                }
            }
        }
        ProjectRequest::Fail { message, reply } => {
            state.sync_runtime(runtime);
            channels.publish_failure(state, message);
            let _ = reply.send(());
        }
        ProjectRequest::Shutdown { reply } => {
            stop_project_runtime(channels, state, runtime, true).await;
            let _ = reply.send(());
            return true;
        }
    }
    false
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
/// Errors returned by the shared project registry.
pub enum ProjectRegistryError {
    /// Dynamic registration state could not be loaded or persisted.
    #[error(transparent)]
    Persistence(#[from] crate::project_persistence::ProjectPersistenceError),
    /// A project identity operation failed while resolving a request path.
    #[error(transparent)]
    Identity(#[from] ProjectIdentityError),
    /// A stable ID was reused for a different canonical root.
    #[error("project ID {id} is already registered for {existing_root}, not {requested_root}")]
    ConflictingProject {
        /// The conflicting stable ID.
        id: ProjectId,
        /// The root currently owned by the ID.
        existing_root: PathBuf,
        /// The newly requested root.
        requested_root: PathBuf,
    },
    /// A different project already owns this canonical root.
    #[error("project root is already registered: {0}")]
    DuplicateRoot(PathBuf),
    /// A compatible linked worktree must use its logical project's stable ID.
    #[error(
        "linked worktree {requested_root} belongs to logical project {existing_id}; use that project ID"
    )]
    LinkedWorktreeProject {
        /// Stable ID of the already-registered logical project.
        existing_id: ProjectId,
        /// Worktree root that was registered under another ID.
        requested_root: PathBuf,
    },
    /// No project with this stable ID is registered.
    #[error("project is not registered: {0}")]
    ProjectNotFound(ProjectId),
    /// A project with this stable ID is already being removed.
    #[error("project is being removed: {0}")]
    ProjectRemoving(ProjectId),
    /// The daemon is draining projects and no new registrations are accepted.
    #[error("project registry is shutting down")]
    ShuttingDown,
    /// The project actor could not service the request.
    #[error(transparent)]
    Actor(#[from] ProjectActorError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProjectCompatibility {
    Deferred,
    Resolved(Option<ProjectCompatibilityKey>),
}

struct ProjectActorEntry {
    actor: ProjectHandle,
    mutation: MutationGate,
    compatibility: ProjectCompatibility,
    translator_template: Option<std::sync::Arc<TranslatorTemplate>>,
    roots: Vec<CanonicalRoot>,
}

impl ProjectActorEntry {
    fn new(
        actor: ProjectHandle,
        mutation: MutationGate,
        compatibility_key: Option<ProjectCompatibilityKey>,
        translator_template: Option<std::sync::Arc<TranslatorTemplate>>,
        root: CanonicalRoot,
    ) -> Self {
        Self {
            actor,
            mutation,
            compatibility: ProjectCompatibility::Resolved(compatibility_key),
            translator_template,
            roots: vec![root],
        }
    }
}

struct ProjectEntry {
    identity: ProjectIdentity,
    actors: Vec<ProjectActorEntry>,
    config: Option<ProjectConfig>,
}

struct ProjectRemovalSnapshot {
    actors: Vec<ProjectHandle>,
    mutations: Vec<MutationGate>,
    root: PathBuf,
}

const RETAINED_PROJECT_HISTORY_CAPACITY: usize = 16;
const RETAINED_LOG_CAPACITY: usize = 100;
const RETAINED_MESSAGE_CAPACITY: usize = 50;

/// Bounded, in-memory history retained after a project is removed.
///
/// Retention is deliberately process-local and limited to the most recent 16
/// removed projects. It is not persisted and is cleared when a project ID is
/// registered again.
#[derive(Debug, Default)]
struct RetainedProjectHistories {
    entries: HashMap<ProjectId, RetainedProjectHistory>,
    order: VecDeque<ProjectId>,
}

#[derive(Debug, Clone, Default)]
struct RetainedProjectHistory {
    logs: Vec<LogEntry>,
    messages: Vec<ServerMessage>,
    capabilities: Vec<ProjectServerCapability>,
}

impl RetainedProjectHistory {
    fn server_logs(
        &self,
        limit: usize,
        min_level: Option<&str>,
    ) -> Result<ServerLogsResult, ProjectActorError> {
        let min_level = min_level
            .map(str::to_ascii_lowercase)
            .map(|level| match level.as_str() {
                "error" => Ok(LogLevel::Error),
                "warning" => Ok(LogLevel::Warning),
                "info" => Ok(LogLevel::Info),
                "debug" => Ok(LogLevel::Debug),
                _ => Err(ProjectActorError::Operation(format!(
                    "Invalid min_level: '{level}'. Valid values: error, warning, info, debug"
                ))),
            })
            .transpose()?;
        Ok(ServerLogsResult {
            logs: self
                .logs
                .iter()
                .filter(|log| {
                    min_level.is_none_or(|min| match min {
                        LogLevel::Error => matches!(log.level, LogLevel::Error),
                        LogLevel::Warning => {
                            matches!(log.level, LogLevel::Error | LogLevel::Warning)
                        }
                        LogLevel::Info => !matches!(log.level, LogLevel::Debug),
                        LogLevel::Debug => true,
                    })
                })
                .take(limit)
                .cloned()
                .collect(),
        })
    }

    fn server_messages(&self, limit: usize) -> ServerMessagesResult {
        ServerMessagesResult {
            messages: self.messages.iter().take(limit).cloned().collect(),
        }
    }
}

impl RetainedProjectHistories {
    fn insert(&mut self, id: ProjectId, history: RetainedProjectHistory) {
        self.entries.remove(&id);
        self.order.retain(|existing| existing != &id);
        self.entries.insert(id.clone(), history);
        self.order.push_back(id);
        while self.order.len() > RETAINED_PROJECT_HISTORY_CAPACITY {
            if let Some(oldest) = self.order.pop_front() {
                self.entries.remove(&oldest);
            }
        }
    }

    fn remove(&mut self, id: &ProjectId) {
        self.entries.remove(id);
        self.order.retain(|existing| existing != id);
    }
}

impl ProjectRemovalSnapshot {
    fn reject_new_work(&self) {
        reject_new_actor_work(&self.actors);
    }

    fn accept_new_work(&self) {
        for actor in &self.actors {
            actor.accept_new_work();
        }
    }

    async fn shutdown(&self, project_id: &ProjectId) -> Result<(), ProjectRegistryError> {
        for actor in &self.actors {
            actor
                .publish_event(ProjectEvent::ProjectRemoved {
                    project_id: project_id.clone(),
                    root: self.root.clone(),
                })
                .await
                .map_err(ProjectRegistryError::from)?;
            actor.shutdown().await.map_err(ProjectRegistryError::from)?;
        }
        Ok(())
    }

    async fn capture_history(&self) -> RetainedProjectHistory {
        let mut history = RetainedProjectHistory::default();
        for (group_id, actor) in self.actors.iter().enumerate() {
            if let Ok(logs) = actor.server_logs_unchecked(usize::MAX, None).await {
                history.logs.extend(logs.logs);
            }
            if let Ok(messages) = actor.server_messages_unchecked(usize::MAX).await {
                history.messages.extend(messages.messages);
            }
            if let Ok(capabilities) = actor.server_capabilities_unchecked(None).await {
                history.capabilities.extend(
                    capabilities.into_iter().map(|capability| {
                        ProjectServerCapability::from_server(group_id, capability)
                    }),
                );
            }
        }
        history
            .logs
            .sort_by_key(|entry| std::cmp::Reverse(entry.timestamp));
        history.logs.truncate(RETAINED_LOG_CAPACITY);
        history
            .messages
            .sort_by_key(|entry| std::cmp::Reverse(entry.timestamp));
        history.messages.truncate(RETAINED_MESSAGE_CAPACITY);
        history
    }
}

impl ProjectEntry {
    fn new(
        identity: ProjectIdentity,
        actor: ProjectHandle,
        mutation: MutationGate,
        compatibility_key: Option<ProjectCompatibilityKey>,
        translator_template: Option<std::sync::Arc<TranslatorTemplate>>,
        config: Option<ProjectConfig>,
    ) -> Self {
        let root = identity.root.clone();
        Self {
            identity,
            actors: vec![ProjectActorEntry::new(
                actor,
                mutation,
                compatibility_key,
                translator_template,
                root,
            )],
            config,
        }
    }

    fn primary(&self) -> &ProjectActorEntry {
        &self.actors[0]
    }

    fn primary_mut(&mut self) -> &mut ProjectActorEntry {
        &mut self.actors[0]
    }

    fn removal_snapshot(&self) -> ProjectRemovalSnapshot {
        let (actors, mutations): (Vec<_>, Vec<_>) = self
            .actors
            .iter()
            .map(|actor| (actor.actor.clone(), actor.mutation.clone()))
            .unzip();
        ProjectRemovalSnapshot {
            actors,
            mutations,
            root: self.identity.root().as_path().to_path_buf(),
        }
    }

    fn actor_for_root(&self, root: &Path) -> Option<&ProjectActorEntry> {
        self.actors.iter().find(|actor| {
            actor
                .roots
                .iter()
                .any(|candidate| candidate.as_path() == root)
        })
    }

    fn compatible_actor(
        &self,
        compatibility_key: Option<ProjectCompatibilityKey>,
    ) -> Option<(ProjectHandle, MutationGate)> {
        let compatibility_key = compatibility_key?;
        self.actors
            .iter()
            .find(|actor| {
                actor.compatibility == ProjectCompatibility::Resolved(Some(compatibility_key))
            })
            .map(|actor| (actor.actor.clone(), actor.mutation.clone()))
    }

    fn has_compatible_actor(&self, compatibility_key: Option<ProjectCompatibilityKey>) -> bool {
        let Some(compatibility_key) = compatibility_key else {
            return false;
        };
        self.actors.iter().any(|actor| {
            actor.compatibility == ProjectCompatibility::Resolved(Some(compatibility_key))
        })
    }

    fn status(&self) -> ProjectStatus {
        aggregate_statuses(
            self.actors
                .iter()
                .map(|actor| *actor.actor.status().borrow()),
        )
    }

    fn status_summary(&self) -> ProjectStatusSummary {
        let mut roots = self
            .actors
            .iter()
            .flat_map(|actor| actor.roots.iter().map(CanonicalRoot::as_path))
            .map(Path::to_path_buf)
            .collect::<Vec<_>>();
        roots.sort();
        roots.dedup();
        ProjectStatusSummary {
            project_id: self.identity.id().clone(),
            status: self.status(),
            actor_group_count: self.actors.len(),
            roots,
        }
    }

    fn queue_pressure(&self) -> ProjectQueuePressure {
        self.actors
            .iter()
            .map(|actor| actor.actor.queue_pressure())
            .fold(ProjectQueuePressure::default(), ProjectQueuePressure::add)
    }
}

type MutationGate = std::sync::Arc<Mutex<()>>;

#[derive(Debug, Default)]
struct RegistryLifecycle {
    shutting_down: AtomicBool,
    removing: Mutex<HashSet<ProjectId>>,
}

const DEFAULT_PROJECT_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30);

impl RegistryLifecycle {
    fn begin_shutdown(&self) {
        self.shutting_down.store(true, Ordering::Release);
    }

    fn ensure_accepting(&self) -> Result<(), ProjectRegistryError> {
        if self.shutting_down.load(Ordering::Acquire) {
            Err(ProjectRegistryError::ShuttingDown)
        } else {
            Ok(())
        }
    }

    async fn ensure_project_available(
        &self,
        project_id: &ProjectId,
    ) -> Result<(), ProjectRegistryError> {
        self.ensure_accepting()?;
        if self.removing.lock().await.contains(project_id) {
            Err(ProjectRegistryError::ProjectRemoving(project_id.clone()))
        } else {
            Ok(())
        }
    }

    async fn begin_removal(&self, project_id: &ProjectId) -> Result<(), ProjectRegistryError> {
        let mut removing = self.removing.lock().await;
        if removing.insert(project_id.clone()) {
            Ok(())
        } else {
            Err(ProjectRegistryError::ProjectRemoving(project_id.clone()))
        }
    }

    async fn end_removal(&self, project_id: &ProjectId) {
        self.removing.lock().await.remove(project_id);
    }
}

/// Process-wide registry of project identities and their actor handles.
#[derive(Clone)]
pub struct ProjectRegistry {
    projects: std::sync::Arc<RwLock<HashMap<ProjectId, ProjectEntry>>>,
    retained_history: std::sync::Arc<RwLock<RetainedProjectHistories>>,
    actor_capacity: usize,
    translator_template: Option<std::sync::Arc<TranslatorTemplate>>,
    persistence: Option<std::sync::Arc<ProjectRegistrationStore>>,
    persistence_error: std::sync::Arc<RwLock<Option<String>>>,
    lifecycle: std::sync::Arc<RegistryLifecycle>,
    shutdown_timeout: Duration,
    rust_residency: RustResidencyController,
    next_rust_group_id: std::sync::Arc<AtomicU64>,
}

/// Bounded lifecycle counts for cheap daemon health reporting.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProjectStatusCounts {
    /// Projects that have not finished activation.
    pub starting: usize,
    /// Projects ready for requests.
    pub ready: usize,
    /// Projects with a degraded component.
    pub degraded: usize,
    /// Projects currently restarting.
    pub restarting: usize,
    /// Registered projects without resident language-server processes.
    pub dormant: usize,
    /// Projects draining before shutdown.
    pub stopping: usize,
    /// Stopped projects still retained by the registry.
    pub stopped: usize,
    /// Failed projects.
    pub failed: usize,
}

/// Cheap lifecycle summary for one registered logical project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectStatusSummary {
    /// Stable project identifier.
    pub project_id: ProjectId,
    /// Aggregate lifecycle status across the project's actor groups.
    pub status: ProjectStatus,
    /// Number of actor groups backing the logical project.
    pub actor_group_count: usize,
    /// Canonical roots owned by the project, sorted and deduplicated.
    pub roots: Vec<PathBuf>,
}

/// Bounded actor request queue usage across the registry snapshot.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProjectQueuePressure {
    /// Requests currently occupying actor queue slots.
    pub queued: usize,
    /// Total bounded request queue slots.
    pub capacity: usize,
}

impl ProjectQueuePressure {
    const fn add(self, other: Self) -> Self {
        Self {
            queued: self.queued + other.queued,
            capacity: self.capacity + other.capacity,
        }
    }
}

/// Coherent, non-blocking snapshot of registered project lifecycle state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectRegistryStatusSnapshot {
    /// Counts by lifecycle state.
    pub counts: ProjectStatusCounts,
    /// Total actor groups across all logical projects.
    pub actor_groups: usize,
    /// Per-project lifecycle summaries.
    pub summaries: Vec<ProjectStatusSummary>,
    /// Aggregate bounded actor queue usage.
    pub queue_pressure: ProjectQueuePressure,
}

/// Negotiated capability data for one actor group in a logical project.
#[derive(Debug, Clone, Serialize)]
pub struct ProjectServerCapability {
    /// Actor-group ordinal within the logical project snapshot.
    pub group_id: usize,
    /// Language ID configured for the server.
    pub language_id: String,
    /// Position encoding negotiated during initialization.
    pub position_encoding: String,
    /// Raw LSP server capabilities.
    pub capabilities: serde_json::Value,
}

impl ProjectServerCapability {
    fn from_server(group_id: usize, capability: ServerCapability) -> Self {
        Self {
            group_id,
            language_id: capability.language_id,
            position_encoding: capability.position_encoding,
            capabilities: capability.capabilities,
        }
    }
}

impl ProjectStatusCounts {
    const fn record(&mut self, status: ProjectStatus) {
        match status {
            ProjectStatus::Starting => self.starting += 1,
            ProjectStatus::Ready => self.ready += 1,
            ProjectStatus::Degraded => self.degraded += 1,
            ProjectStatus::Restarting => self.restarting += 1,
            ProjectStatus::Dormant => self.dormant += 1,
            ProjectStatus::Stopping => self.stopping += 1,
            ProjectStatus::Stopped => self.stopped += 1,
            ProjectStatus::Failed => self.failed += 1,
        }
    }
}

/// Result of a bounded daemon shutdown across all registered projects.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProjectShutdownReport {
    /// Project IDs whose actor reached `Stopped` (including already-stopped actors).
    pub stopped: Vec<ProjectId>,
    /// Projects whose actor could not be shut down cleanly.
    pub failed: Vec<ProjectShutdownFailure>,
}

/// One project shutdown failure and its actor error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectShutdownFailure {
    /// Project ID associated with the failed actor.
    pub project_id: ProjectId,
    /// Human-readable shutdown failure.
    pub error: String,
}

impl ProjectShutdownReport {
    fn record_actor_result(
        &mut self,
        project_ids: Vec<ProjectId>,
        result: Result<(), ProjectActorError>,
    ) {
        match result {
            Ok(()) => self.stopped.extend(project_ids),
            Err(error) => self
                .failed
                .extend(
                    project_ids
                        .into_iter()
                        .map(|project_id| ProjectShutdownFailure {
                            project_id,
                            error: error.to_string(),
                        }),
                ),
        }
    }

    fn record_actor_timeout(&mut self, project_ids: Vec<ProjectId>, timeout: Duration) {
        self.failed.extend(
            project_ids
                .into_iter()
                .map(|project_id| ProjectShutdownFailure {
                    project_id,
                    error: format!("shutdown timed out after {timeout:?}"),
                }),
        );
    }

    fn sort(&mut self) {
        self.stopped.sort();
        self.failed
            .sort_by(|left, right| left.project_id.cmp(&right.project_id));
    }
}

enum ShutdownAttempt {
    Completed(Result<(), ProjectActorError>),
    TimedOut,
}

async fn shutdown_actor_with_timeout(actor: ProjectHandle, timeout: Duration) -> ShutdownAttempt {
    tokio::time::timeout(timeout, actor.shutdown())
        .await
        .map_or(ShutdownAttempt::TimedOut, ShutdownAttempt::Completed)
}

impl ProjectRegistry {
    fn spawn_actor(
        &self,
        root: &CanonicalRoot,
        translator_template: Option<&TranslatorTemplate>,
    ) -> ProjectHandle {
        let Some(template) = translator_template else {
            return spawn_project_actor_for_root(self.actor_capacity, root);
        };
        let residency = template
            .language_applies_to_root("rust", root.as_path())
            .then(|| ProjectResidency {
                controller: self.rust_residency.clone(),
                group: RustGroupId(self.next_rust_group_id.fetch_add(1, Ordering::Relaxed)),
            });
        spawn_project_actor_with_runtime(
            self.actor_capacity,
            template.translator_for_root(root.as_path().to_path_buf()),
            template.edit_safety().cloned(),
            residency,
        )
    }

    fn with_template(
        actor_capacity: usize,
        translator_template: Option<TranslatorTemplate>,
    ) -> Self {
        Self {
            projects: std::sync::Arc::new(RwLock::new(HashMap::new())),
            retained_history: std::sync::Arc::new(RwLock::new(RetainedProjectHistories::default())),
            actor_capacity: actor_capacity.max(1),
            translator_template: translator_template.map(std::sync::Arc::new),
            persistence: None,
            persistence_error: std::sync::Arc::new(RwLock::new(None)),
            lifecycle: std::sync::Arc::new(RegistryLifecycle::default()),
            shutdown_timeout: DEFAULT_PROJECT_SHUTDOWN_TIMEOUT,
            rust_residency: RustResidencyController::new(1),
            next_rust_group_id: std::sync::Arc::new(AtomicU64::new(1)),
        }
    }

    /// Create an empty registry with a bounded actor queue capacity.
    #[must_use]
    pub fn new(actor_capacity: usize) -> Self {
        Self::with_template(actor_capacity, None)
    }

    /// Create a registry whose actors inherit only the daemon translator's configuration.
    #[must_use]
    pub fn with_translator_template(actor_capacity: usize, template: TranslatorTemplate) -> Self {
        Self::with_template(actor_capacity, Some(template))
    }

    /// Set the maximum time allowed for each actor shutdown request.
    #[must_use]
    pub const fn with_shutdown_timeout(mut self, timeout: Duration) -> Self {
        self.shutdown_timeout = timeout;
        self
    }

    /// Set the process-wide resident rust-analyzer group limit.
    #[must_use]
    pub fn with_rust_residency_limit(mut self, limit: usize) -> Self {
        self.rust_residency = RustResidencyController::new(limit);
        self
    }

    /// Attach a durable registration store to this registry.
    #[must_use]
    pub fn with_persistence(mut self, store: ProjectRegistrationStore) -> Self {
        self.persistence = Some(std::sync::Arc::new(store));
        self
    }

    async fn persist(&self) -> Result<(), ProjectRegistryError> {
        let Some(store) = self.persistence.clone() else {
            return Ok(());
        };
        let mut projects = self
            .projects
            .read()
            .await
            .values()
            .map(|project| {
                PersistedProject::from_identity_with_config(
                    &project.identity,
                    project.config.clone(),
                )
            })
            .collect::<Vec<_>>();
        projects.sort_by(|left, right| left.project_id.cmp(&right.project_id));
        let result = save_persisted_state(store, projects).await;
        self.record_persistence_error(result.as_ref().err().map(ToString::to_string))
            .await;
        result
    }

    async fn record_persistence_error(&self, error: Option<String>) {
        *self.persistence_error.write().await = error;
    }

    /// Restore valid registrations from the attached store.
    ///
    /// Missing or moved roots are skipped and are removed from the next
    /// successful save; no language server is activated during restoration.
    ///
    /// # Errors
    ///
    /// Returns an error when the store cannot be loaded or a valid registration
    /// cannot be added to the registry.
    pub async fn restore_from_persistence(&self) -> Result<usize, ProjectRegistryError> {
        let Some(store) = self.persistence.clone() else {
            return Ok(0);
        };
        let state = match load_persisted_state(store).await {
            Ok(state) => {
                self.record_persistence_error(None).await;
                state
            }
            Err(error) => {
                self.record_persistence_error(Some(error.to_string())).await;
                return Err(error);
            }
        };
        let mut restored = 0;
        for persisted in state.projects {
            let Ok(id) = persisted.project_id() else {
                continue;
            };
            let Ok(root) = CanonicalRoot::new(&persisted.root) else {
                continue;
            };
            let identity = match GitRepositoryIdentity::discover(root.as_path()) {
                Ok(Some(repository)) => {
                    ProjectIdentity::new(id.clone(), root).with_repository_identity(repository)
                }
                _ => ProjectIdentity::new(id.clone(), root),
            };
            self.add_restored_with_config(identity, persisted.config.clone())
                .await?;
            for additional_root in &persisted.additional_roots {
                let Ok(additional_root) = CanonicalRoot::new(additional_root) else {
                    continue;
                };
                let Ok(Some(repository)) =
                    GitRepositoryIdentity::discover(additional_root.as_path())
                else {
                    continue;
                };
                self.add_with_config(
                    ProjectIdentity::new(id.clone(), additional_root)
                        .with_repository_identity(repository),
                    persisted.config.clone(),
                )
                .await?;
            }
            restored += 1;
        }
        self.persist().await?;
        Ok(restored)
    }

    /// Add a logical project, sharing actors only with compatible worktrees.
    ///
    /// # Errors
    ///
    /// Returns [`ProjectRegistryError::DuplicateRoot`] when another project owns the root.
    #[allow(clippy::too_many_lines)]
    pub async fn add(
        &self,
        identity: ProjectIdentity,
    ) -> Result<ProjectHandle, ProjectRegistryError> {
        self.add_registration(identity, None, None, false).await
    }

    /// Add a project with an optional JSON-facing configuration override.
    ///
    /// # Errors
    ///
    /// Returns an error if project identity, compatibility, actor, or
    /// persistence validation fails.
    pub async fn add_with_config(
        &self,
        identity: ProjectIdentity,
        config: Option<ProjectConfig>,
    ) -> Result<ProjectHandle, ProjectRegistryError> {
        self.add_configured_registration(identity, config, false)
            .await
    }

    async fn add_restored_with_config(
        &self,
        identity: ProjectIdentity,
        config: Option<ProjectConfig>,
    ) -> Result<ProjectHandle, ProjectRegistryError> {
        self.add_configured_registration(identity, config, true)
            .await
    }

    async fn add_configured_registration(
        &self,
        identity: ProjectIdentity,
        config: Option<ProjectConfig>,
        defer_compatibility: bool,
    ) -> Result<ProjectHandle, ProjectRegistryError> {
        let config = config.filter(|config| !config.is_empty());
        let template = config.as_ref().map(|config| {
            self.translator_template
                .as_deref()
                .cloned()
                .unwrap_or_default()
                .with_project_config(config)
        });
        self.add_registration(identity, config, template, defer_compatibility)
            .await
    }

    /// Add a project with an optional runtime translator configuration.
    ///
    /// When no override is supplied, actors inherit the daemon template. A
    /// project ID/root pair can only be reused with the same effective
    /// translator configuration.
    ///
    /// # Errors
    ///
    /// Returns an error if project identity, compatibility, actor, or
    /// persistence validation fails.
    #[allow(clippy::too_many_lines)]
    pub async fn add_with_template(
        &self,
        identity: ProjectIdentity,
        translator_template: Option<TranslatorTemplate>,
    ) -> Result<ProjectHandle, ProjectRegistryError> {
        self.add_registration(identity, None, translator_template, false)
            .await
    }

    #[allow(clippy::too_many_lines)]
    async fn add_registration(
        &self,
        identity: ProjectIdentity,
        config: Option<ProjectConfig>,
        translator_template: Option<TranslatorTemplate>,
        defer_compatibility: bool,
    ) -> Result<ProjectHandle, ProjectRegistryError> {
        let translator_template = translator_template
            .map(std::sync::Arc::new)
            .or_else(|| self.translator_template.clone());
        self.resolve_deferred_compatibility_keys(identity.repository_identity())
            .await;
        let repository_registered = {
            let projects = self.projects.read().await;
            identity.repository_identity().is_some_and(|repository| {
                projects
                    .values()
                    .any(|project| project.identity.repository_identity() == Some(repository))
            })
        };
        let defer_compatibility = defer_compatibility && !repository_registered;
        let compatibility_key = if defer_compatibility {
            None
        } else {
            rust_project_compatibility_key(identity.root.as_path(), translator_template.as_deref())
                .await
        };
        let mut projects = self.projects.write().await;
        self.lifecycle
            .ensure_project_available(identity.id())
            .await?;
        if let Some(existing) = projects.get(identity.id()) {
            if let Some(actor) = existing.actor_for_root(identity.root().as_path()) {
                if translator_templates_match(
                    actor.translator_template.as_deref(),
                    translator_template.as_deref(),
                ) {
                    return Ok(actor.actor.clone());
                }
                return Err(ProjectRegistryError::ConflictingProject {
                    id: identity.id().clone(),
                    existing_root: identity.root().as_path().to_path_buf(),
                    requested_root: identity.root().as_path().to_path_buf(),
                });
            }
            let compatible = (existing.identity.repository_identity()
                == identity.repository_identity())
            .then(|| existing.compatible_actor(compatibility_key))
            .flatten();
            if let Some((actor, mutation)) = compatible {
                drop(projects);
                let mutation_guard = mutation.lock().await;
                self.lifecycle.ensure_accepting()?;
                actor
                    .add_workspace_root(identity.root().as_path().to_path_buf())
                    .await?;
                let mut projects = self.projects.write().await;
                if let Some(existing) = projects.get_mut(identity.id()) {
                    existing.identity.add_root(identity.root.clone());
                    if let Some(actor) = existing
                        .actors
                        .iter_mut()
                        .find(|entry| entry.actor.sender.same_channel(&actor.sender))
                    {
                        actor.roots.push(identity.root.clone());
                    }
                }
                drop(projects);
                drop(mutation_guard);
                self.persist().await?;
                return Ok(actor);
            }
            if existing.identity.repository_identity().is_none()
                || identity.repository_identity().is_none()
                || compatibility_key.is_none()
                || existing.identity.repository_identity() != identity.repository_identity()
            {
                return Err(ProjectRegistryError::ConflictingProject {
                    id: identity.id().clone(),
                    existing_root: existing.identity.root().as_path().to_path_buf(),
                    requested_root: identity.root().as_path().to_path_buf(),
                });
            }
            let actor = self.spawn_actor(identity.root(), translator_template.as_deref());
            let mutation = std::sync::Arc::new(Mutex::new(()));
            drop(projects);
            let mut projects = self.projects.write().await;
            if let Some(existing) = projects.get_mut(identity.id()) {
                existing.identity.add_root(identity.root.clone());
                existing.actors.push(ProjectActorEntry::new(
                    actor.clone(),
                    mutation,
                    compatibility_key,
                    translator_template.clone(),
                    identity.root.clone(),
                ));
            }
            drop(projects);
            self.persist().await?;
            return Ok(actor);
        }
        if projects
            .values()
            .flat_map(|project| project.identity.roots())
            .any(|root| root == identity.root())
        {
            return Err(ProjectRegistryError::DuplicateRoot(
                identity.root().as_path().to_path_buf(),
            ));
        }

        if let Some(existing) = compatible_project(&projects, &identity, compatibility_key) {
            return Err(ProjectRegistryError::LinkedWorktreeProject {
                existing_id: existing.identity.id().clone(),
                requested_root: identity.root().as_path().to_path_buf(),
            });
        }

        let primary_root = identity.root.clone();
        let project_id = identity.id().clone();
        let actor = self.spawn_actor(&primary_root, translator_template.as_deref());
        let mutation = std::sync::Arc::new(Mutex::new(()));
        let mut entry = ProjectEntry::new(
            identity,
            actor.clone(),
            mutation,
            compatibility_key,
            translator_template,
            config,
        );
        if defer_compatibility {
            entry.primary_mut().compatibility = ProjectCompatibility::Deferred;
        }
        projects.insert(project_id.clone(), entry);
        drop(projects);
        self.retained_history.write().await.remove(&project_id);
        self.persist().await?;
        Ok(actor)
    }

    async fn resolve_deferred_compatibility_keys(
        &self,
        repository: Option<&GitRepositoryIdentity>,
    ) {
        let Some(repository) = repository else {
            return;
        };
        let pending = {
            let projects = self.projects.read().await;
            projects
                .values()
                .filter(|project| project.identity.repository_identity() == Some(repository))
                .flat_map(|project| &project.actors)
                .filter(|actor| matches!(actor.compatibility, ProjectCompatibility::Deferred))
                .map(|actor| {
                    (
                        actor.actor.clone(),
                        actor.roots[0].as_path().to_path_buf(),
                        actor.translator_template.clone(),
                    )
                })
                .collect::<Vec<_>>()
        };
        let mut resolved = Vec::with_capacity(pending.len());
        for (actor, root, translator_template) in pending {
            let key = rust_project_compatibility_key(&root, translator_template.as_deref()).await;
            resolved.push((actor, key));
        }
        if resolved.is_empty() {
            return;
        }
        let mut projects = self.projects.write().await;
        for (handle, key) in resolved {
            for actor in projects
                .values_mut()
                .filter(|project| project.identity.repository_identity() == Some(repository))
                .flat_map(|project| &mut project.actors)
                .filter(|actor| actor.actor.sender.same_channel(&handle.sender))
            {
                actor.compatibility = ProjectCompatibility::Resolved(key);
            }
        }
    }

    /// List registered project identities without waiting on any actor.
    pub async fn list(&self) -> Vec<ProjectIdentity> {
        let mut projects: Vec<_> = self
            .projects
            .read()
            .await
            .values()
            .map(|project| project.identity.clone())
            .collect();
        projects.sort_by(|left, right| left.id().cmp(right.id()));
        projects
    }

    /// Read lifecycle watches without awaiting any actor request.
    pub async fn status_counts(&self) -> ProjectStatusCounts {
        let projects = self.projects.read().await;
        let mut counts = ProjectStatusCounts::default();
        for entry in projects.values() {
            counts.record(entry.status());
        }
        drop(projects);
        counts
    }

    /// Read project lifecycle summaries without awaiting any actor request.
    pub async fn status_summaries(&self) -> Vec<ProjectStatusSummary> {
        let projects = self.projects.read().await;
        let mut summaries: Vec<ProjectStatusSummary> = projects
            .values()
            .map(ProjectEntry::status_summary)
            .collect();
        drop(projects);
        summaries.sort_by(|left, right| left.project_id.cmp(&right.project_id));
        summaries
    }

    /// Read one coherent lifecycle snapshot without awaiting any actor request.
    pub async fn status_snapshot(&self) -> ProjectRegistryStatusSnapshot {
        let projects = self.projects.read().await;
        let mut snapshot = ProjectRegistryStatusSnapshot {
            counts: ProjectStatusCounts::default(),
            actor_groups: 0,
            summaries: Vec::with_capacity(projects.len()),
            queue_pressure: ProjectQueuePressure::default(),
        };
        for entry in projects.values() {
            snapshot.counts.record(entry.status());
            snapshot.actor_groups += entry.actors.len();
            snapshot.summaries.push(entry.status_summary());
            snapshot.queue_pressure = snapshot.queue_pressure.add(entry.queue_pressure());
        }
        drop(projects);
        snapshot
            .summaries
            .sort_by(|left, right| left.project_id.cmp(&right.project_id));
        snapshot
    }

    /// Return whether durable project registration is configured.
    #[must_use]
    pub const fn persistence_configured(&self) -> bool {
        self.persistence.is_some()
    }

    /// Return the most recent persistence error, if any.
    pub async fn persistence_error(&self) -> Option<String> {
        self.persistence_error.read().await.clone()
    }

    /// Return whether the registry is draining during daemon shutdown.
    #[must_use]
    pub fn is_shutting_down(&self) -> bool {
        self.lifecycle.shutting_down.load(Ordering::Acquire)
    }

    /// Count actor groups without awaiting any actor request.
    #[must_use]
    pub async fn total_actor_group_count(&self) -> usize {
        self.projects
            .read()
            .await
            .values()
            .map(|project| project.actors.len())
            .sum()
    }

    /// Gracefully stop every registered project actor once.
    ///
    /// Requests already queued on an actor are processed before its shutdown
    /// request, preserving edit commit boundaries without holding the registry
    /// lock across the await.
    pub async fn shutdown_all(&self) -> ProjectShutdownReport {
        self.lifecycle.begin_shutdown();
        let entries = self.registered_actor_entries().await;

        reject_new_actor_work(entries.iter().map(|(_, actor)| actor));

        let _mutation_guards = self.lock_project_mutations().await;

        let (stopped, actors) = shutdown_actor_groups(entries);
        let mut report = ProjectShutdownReport {
            stopped,
            failed: Vec::new(),
        };

        for (actor, project_ids) in actors {
            match shutdown_actor_with_timeout(actor, self.shutdown_timeout).await {
                ShutdownAttempt::Completed(result) => {
                    report.record_actor_result(project_ids, result);
                }
                ShutdownAttempt::TimedOut => {
                    report.record_actor_timeout(project_ids, self.shutdown_timeout);
                }
            }
        }

        report.sort();
        report
    }

    async fn lock_project_mutations(&self) -> Vec<tokio::sync::OwnedMutexGuard<()>> {
        let mutations = {
            let projects = self.projects.read().await;
            unique_mutation_gates(&projects)
        };
        self.lock_mutation_gates(mutations).await
    }

    async fn registered_actor_entries(&self) -> Vec<(ProjectId, ProjectHandle)> {
        self.projects
            .read()
            .await
            .values()
            .flat_map(|entry| {
                entry
                    .actors
                    .iter()
                    .map(move |actor| (entry.identity.id().clone(), actor.actor.clone()))
            })
            .collect()
    }

    async fn lock_mutation_gates(
        &self,
        mutations: Vec<MutationGate>,
    ) -> Vec<tokio::sync::OwnedMutexGuard<()>> {
        let mut guards = Vec::with_capacity(mutations.len());
        for mutation in mutations {
            guards.push(mutation.lock_owned().await);
        }
        guards
    }

    /// Return open-document paths grouped by the registered project IDs that
    /// own them. Actor state is queried after the registry lock is released.
    ///
    /// # Errors
    ///
    /// Returns an error if an actor closes while its document state is queried.
    pub async fn open_document_paths(
        &self,
    ) -> Result<Vec<(ProjectId, PathBuf)>, ProjectRegistryError> {
        let entries = self.registered_actor_entries().await;
        let mut paths = Vec::new();
        for (id, actor) in entries {
            paths.extend(
                actor
                    .open_document_paths()
                    .await
                    .map_err(ProjectRegistryError::from)?
                    .into_iter()
                    .map(|path| (id.clone(), path)),
            );
        }
        paths.sort();
        paths.dedup();
        Ok(paths)
    }

    async fn begin_project_removal(
        &self,
        id: &ProjectId,
    ) -> Result<ProjectRemovalSnapshot, ProjectRegistryError> {
        let projects = self.projects.read().await;
        let entry = projects
            .get(id)
            .ok_or_else(|| ProjectRegistryError::ProjectNotFound(id.clone()))?;
        self.lifecycle.begin_removal(id).await?;
        let removal = entry.removal_snapshot();
        drop(projects);
        removal.reject_new_work();
        Ok(removal)
    }

    async fn abort_project_removal(
        &self,
        id: &ProjectId,
        removal: &ProjectRemovalSnapshot,
        error: ProjectRegistryError,
    ) -> Result<(), ProjectRegistryError> {
        removal.accept_new_work();
        self.lifecycle.end_removal(id).await;
        Err(error)
    }

    async fn retain_history(&self, id: ProjectId, history: RetainedProjectHistory) {
        self.retained_history.write().await.insert(id, history);
    }

    async fn retained_history_for(
        &self,
        id: &ProjectId,
    ) -> Result<RetainedProjectHistory, ProjectRegistryError> {
        self.retained_history
            .read()
            .await
            .entries
            .get(id)
            .cloned()
            .ok_or_else(|| ProjectRegistryError::ProjectNotFound(id.clone()))
    }

    /// Remove a project and shut down its actor when no linked project remains.
    ///
    /// # Errors
    ///
    /// Returns an error when the project is not registered or its actor cannot shut down.
    pub async fn remove(&self, id: ProjectId) -> Result<(), ProjectRegistryError> {
        let removal = self.begin_project_removal(&id).await?;
        let _mutation_guards = self.lock_mutation_gates(removal.mutations.clone()).await;
        let history = removal.capture_history().await;
        if let Err(error) = removal.shutdown(&id).await {
            return self.abort_project_removal(&id, &removal, error).await;
        }
        if self.projects.write().await.remove(&id).is_none() {
            self.lifecycle.end_removal(&id).await;
            return Err(ProjectRegistryError::ProjectNotFound(id));
        }
        self.retain_history(id.clone(), history).await;
        let persisted = self.persist().await;
        self.lifecycle.end_removal(&id).await;
        persisted
    }

    /// Query a project's actor state without holding the registry lock during the await.
    ///
    /// # Errors
    ///
    /// Returns an error when the project is not registered or its actor is unavailable.
    pub async fn status(&self, id: &ProjectId) -> Result<ProjectState, ProjectRegistryError> {
        let (_, actors) = self.actor_entries(id).await?;
        let mut states = Vec::with_capacity(actors.len());
        for (actor, _) in actors {
            states.push(actor.query().await?);
        }
        Ok(ProjectState::aggregate(states))
    }

    /// Return negotiated capabilities from every active actor group.
    ///
    /// # Errors
    ///
    /// Returns an error when the project is not registered or an actor cannot
    /// service the capability request.
    pub async fn server_capabilities(
        &self,
        id: &ProjectId,
        language_id: Option<String>,
    ) -> Result<Vec<ProjectServerCapability>, ProjectRegistryError> {
        let actors = match self.actor_entries(id).await {
            Ok((_, actors)) => actors,
            Err(ProjectRegistryError::ProjectNotFound(_)) => {
                return Ok(self
                    .retained_history_for(id)
                    .await?
                    .capabilities
                    .into_iter()
                    .filter(|capability| {
                        language_id
                            .as_deref()
                            .is_none_or(|language| capability.language_id == language)
                    })
                    .collect());
            }
            Err(error) => return Err(error),
        };
        let mut capabilities = Vec::new();
        for (group_id, (actor, _)) in actors.into_iter().enumerate() {
            for capability in actor.server_capabilities(language_id.clone()).await? {
                capabilities.push(ProjectServerCapability::from_server(group_id, capability));
            }
        }
        capabilities.sort_by(|left, right| {
            left.group_id
                .cmp(&right.group_id)
                .then_with(|| left.language_id.cmp(&right.language_id))
        });
        Ok(capabilities)
    }

    /// Return recent logs from a project's primary actor.
    ///
    /// # Errors
    ///
    /// Returns an error when the project is not registered, its actor closes,
    /// or the requested log-level filter is invalid.
    pub async fn server_logs(
        &self,
        id: &ProjectId,
        limit: usize,
        min_level: Option<String>,
    ) -> Result<ServerLogsResult, ProjectRegistryError> {
        match self.actor(id).await {
            Ok(actor) => actor
                .server_logs(limit, min_level)
                .await
                .map_err(ProjectRegistryError::from),
            Err(ProjectRegistryError::ProjectNotFound(_)) => self
                .retained_history_for(id)
                .await?
                .server_logs(limit, min_level.as_deref())
                .map_err(ProjectRegistryError::from),
            Err(error) => Err(error),
        }
    }

    /// Return recent messages from a project's primary actor.
    ///
    /// # Errors
    ///
    /// Returns an error when the project is not registered or its actor closes.
    pub async fn server_messages(
        &self,
        id: &ProjectId,
        limit: usize,
    ) -> Result<ServerMessagesResult, ProjectRegistryError> {
        match self.actor(id).await {
            Ok(actor) => actor
                .server_messages(limit)
                .await
                .map_err(ProjectRegistryError::from),
            Err(ProjectRegistryError::ProjectNotFound(_)) => {
                Ok(self.retained_history_for(id).await?.server_messages(limit))
            }
            Err(error) => Err(error),
        }
    }

    /// List code actions with project-owned opaque references.
    ///
    /// # Errors
    ///
    /// Returns an error when the project or file is not registered, or when
    /// the actor rejects the request.
    #[allow(clippy::too_many_arguments)]
    pub async fn code_action_list(
        &self,
        id: &ProjectId,
        file_path: String,
        start_line: u32,
        start_character: u32,
        end_line: u32,
        end_character: u32,
        kind_filter: Option<String>,
    ) -> Result<CodeActionsResult, ProjectRegistryError> {
        let (identity, actor, _) = self.entry(id).await?;
        let path = canonicalize(Path::new(&file_path))?;
        if !path.starts_with(identity.root().as_path()) {
            return Err(ProjectIdentityError::ProjectPathMismatch {
                id: id.clone(),
                path,
            }
            .into());
        }
        actor
            .code_action_list(
                file_path,
                start_line,
                start_character,
                end_line,
                end_character,
                kind_filter,
            )
            .await
            .map_err(ProjectRegistryError::from)
    }

    pub(crate) async fn path_rename_preview(
        &self,
        id: &ProjectId,
        request: PathRenameRequest,
    ) -> Result<PathRenamePreview, ProjectRegistryError> {
        let (identity, actor, mutation) = self.entry(id).await?;
        let path = canonicalize(Path::new(&request.old_path))?;
        let roots = identity
            .roots()
            .iter()
            .map(CanonicalRoot::as_path)
            .map(Path::to_path_buf)
            .collect::<Vec<_>>();
        let root = longest_matching_root(&path, &roots)
            .ok_or_else(|| ProjectIdentityError::ProjectPathMismatch {
                id: id.clone(),
                path: path.clone(),
            })?
            .to_path_buf();
        let _mutation = mutation.lock().await;
        actor
            .path_rename_preview(id.as_str().to_string(), request, root)
            .await
            .map_err(ProjectRegistryError::from)
    }

    /// Preview one project-owned code-action reference.
    ///
    /// # Errors
    ///
    /// Returns an error when the project is not registered or the action
    /// cannot be resolved and safely previewed.
    pub async fn preview_code_action(
        &self,
        id: &ProjectId,
        action_id: PlanId,
        encoding: PositionEncoding,
    ) -> Result<PreviewArtifact, ProjectRegistryError> {
        let (identity, actor, mutation) = self.entry(id).await?;
        let _mutation = mutation.lock().await;
        actor
            .preview_code_action(
                action_id,
                id.as_str().to_string(),
                encoding,
                identity.root().as_path().to_path_buf(),
            )
            .await
            .map_err(ProjectRegistryError::from)
    }

    /// Activate a registered project's actor-owned language servers.
    ///
    /// # Errors
    ///
    /// Returns an error when the project is not registered or activation fails.
    pub async fn activate(&self, id: &ProjectId) -> Result<ProjectState, ProjectRegistryError> {
        let (_, actors) = self.actor_entries(id).await?;
        let _mutations = self
            .lock_mutation_gates(
                actors
                    .iter()
                    .map(|(_, mutation)| mutation.clone())
                    .collect(),
            )
            .await;
        for (actor, _) in &actors {
            let roots = actor.query().await?.workspace_roots().to_vec();
            if roots.len() > 1 {
                actor.activate_workspace_roots(roots).await?;
            } else if let Some(root) = roots.into_iter().next() {
                actor.activate(root).await?;
            }
        }
        actors[0]
            .0
            .query()
            .await
            .map_err(ProjectRegistryError::from)
    }

    /// Mark a registered project ready after its language servers are loaded.
    ///
    /// # Errors
    ///
    /// Returns an error when the project is not registered or its actor is unavailable.
    pub async fn mark_ready(&self, id: &ProjectId) -> Result<ProjectState, ProjectRegistryError> {
        let actor = self.actor(id).await?;
        actor
            .set_status(ProjectStatus::Ready)
            .await
            .map_err(ProjectRegistryError::from)?;
        actor.query().await.map_err(ProjectRegistryError::from)
    }

    /// Return a registered project's identity without waiting on its actor.
    ///
    /// # Errors
    ///
    /// Returns an error when the project is not registered.
    pub async fn identity(&self, id: &ProjectId) -> Result<ProjectIdentity, ProjectRegistryError> {
        self.projects
            .read()
            .await
            .get(id)
            .map(|project| project.identity.clone())
            .ok_or_else(|| ProjectRegistryError::ProjectNotFound(id.clone()))
    }

    /// Refresh a project's actor state.
    ///
    /// # Errors
    ///
    /// Returns an error when the project is not registered or its actor is unavailable.
    pub async fn refresh(&self, id: &ProjectId) -> Result<ProjectState, ProjectRegistryError> {
        self.actor(id)
            .await?
            .refresh()
            .await
            .map_err(ProjectRegistryError::from)
    }

    /// Restart a project's actor-managed services.
    ///
    /// # Errors
    ///
    /// Returns an error when the project is not registered or its actor is unavailable.
    pub async fn restart(&self, id: &ProjectId) -> Result<ProjectState, ProjectRegistryError> {
        let (_, actors) = self.actor_entries(id).await?;
        let _mutations = self
            .lock_mutation_gates(
                actors
                    .iter()
                    .map(|(_, mutation)| mutation.clone())
                    .collect(),
            )
            .await;
        for (actor, _) in &actors {
            actor.restart().await?;
        }
        actors[0]
            .0
            .query()
            .await
            .map_err(ProjectRegistryError::from)
    }

    /// Consume and apply a project-owned edit plan under the registry's
    /// project mutation gate.
    ///
    /// # Errors
    ///
    /// Returns an error when the project is not registered, the plan is not
    /// owned by it, or filesystem validation/application fails.
    pub async fn apply_edit_plan(
        &self,
        id: &ProjectId,
        plan_id: PlanId,
    ) -> Result<AppliedEditPlan, ProjectRegistryError> {
        self.apply_edit_plan_with_context(id, plan_id, None, None)
            .await
    }

    /// Consume and apply a project-owned edit plan while recording audit context.
    ///
    /// # Errors
    ///
    /// Returns an error when the project is not registered, the plan is not
    /// owned by it, or filesystem validation/application fails.
    pub async fn apply_edit_plan_with_context(
        &self,
        id: &ProjectId,
        plan_id: PlanId,
        session_id: Option<String>,
        principal: Option<String>,
    ) -> Result<AppliedEditPlan, ProjectRegistryError> {
        let (identity, actor, mutation) = self.entry(id).await?;
        let _mutation = mutation.lock().await;
        actor
            .apply_edit_plan_with_context(
                plan_id,
                id.as_str().to_string(),
                identity.root().as_path().to_path_buf(),
                session_id,
                principal,
            )
            .await
            .map_err(ProjectRegistryError::from)
    }

    /// Preview and retain a project-owned LSP workspace edit under the
    /// registry's project mutation gate.
    ///
    /// # Errors
    ///
    /// Returns an error when the project is not registered, preview
    /// validation fails, or the bounded plan store rejects the artifact.
    pub async fn preview_edit(
        &self,
        id: &ProjectId,
        edit: WorkspaceEdit,
        encoding: PositionEncoding,
    ) -> Result<PreviewArtifact, ProjectRegistryError> {
        let (identity, actor, mutation) = self.entry(id).await?;
        let _mutation = mutation.lock().await;
        actor
            .preview_edit(
                id.as_str().to_string(),
                edit,
                encoding,
                identity.root().as_path().to_path_buf(),
            )
            .await
            .map_err(ProjectRegistryError::from)
    }

    /// Preview an inline Rust module move using the actor's current document state.
    ///
    /// The source path validation, dirty-document lookup, AST extraction, and
    /// generic edit preview are serialized behind the same project mutation gate.
    ///
    /// # Errors
    ///
    /// Returns an error when the project is not registered, the source path is
    /// outside the project, the module cannot be extracted safely, or the edit
    /// preview fails its normal workspace checks.
    pub async fn preview_inline_module_move(
        &self,
        id: &ProjectId,
        file_path: String,
        module_name: String,
        module_position: Option<lsp_types::Position>,
        encoding: PositionEncoding,
    ) -> Result<PreviewArtifact, ProjectRegistryError> {
        let (identity, actor, mutation) = self.entry(id).await?;
        let _mutation = mutation.lock().await;
        actor
            .move_inline_module_preview(
                id.as_str().to_string(),
                file_path,
                module_name,
                module_position,
                encoding,
                identity.root().as_path().to_path_buf(),
            )
            .await
            .map_err(ProjectRegistryError::from)
    }

    /// Search or preview an explicitly selected structural replacement dialect.
    pub(crate) async fn structural_replace_preview(
        &self,
        id: &ProjectId,
        request: StructuralReplaceRequest,
    ) -> Result<StructuralPreview, ProjectRegistryError> {
        let (identity, actor, mutation) = self.entry(id).await?;
        let path = canonicalize(Path::new(&request.file_path))?;
        let roots = identity
            .roots()
            .iter()
            .map(CanonicalRoot::as_path)
            .map(Path::to_path_buf)
            .collect::<Vec<_>>();
        let root = longest_matching_root(&path, &roots)
            .ok_or_else(|| ProjectIdentityError::ProjectPathMismatch {
                id: id.clone(),
                path: path.clone(),
            })?
            .to_path_buf();
        let _mutation = mutation.lock().await;
        actor
            .structural_replace_preview(id.as_str().to_string(), request, root)
            .await
            .map_err(ProjectRegistryError::from)
    }

    /// Resolve a file path to the actor owning the longest matching root.
    ///
    /// The registry lock is released before the returned actor is used, so a
    /// slow semantic request cannot block unrelated project registration.
    ///
    /// # Errors
    ///
    /// Returns an identity error when the path cannot be canonicalized or is
    /// not contained by a registered project.
    pub async fn actor_for_path(
        &self,
        path: impl AsRef<Path>,
    ) -> Result<ProjectHandle, ProjectRegistryError> {
        self.project_for_path(path).await.map(|(_, actor)| actor)
    }

    /// Resolve a file path to its owning project ID and actor.
    ///
    /// This is the identity-preserving form of [`Self::actor_for_path`], used
    /// by session event sinks that must keep subscriptions scoped to one
    /// project actor.
    ///
    /// # Errors
    ///
    /// Returns an identity error when the path cannot be canonicalized or is
    /// not contained by a registered project.
    pub async fn project_for_path(
        &self,
        path: impl AsRef<Path>,
    ) -> Result<(ProjectId, ProjectHandle), ProjectRegistryError> {
        let canonical = canonicalize(path.as_ref())?;
        self.projects
            .read()
            .await
            .values()
            .filter_map(|project| {
                project
                    .identity
                    .roots()
                    .iter()
                    .filter(|root| canonical.starts_with(root.as_path()))
                    .max_by_key(|root| root.as_path().components().count())
                    .and_then(|root| {
                        project.actor_for_root(root.as_path()).map(|actor| {
                            (
                                root.as_path().components().count(),
                                project.identity.id().clone(),
                                actor.actor.clone(),
                            )
                        })
                    })
            })
            .max_by_key(|(components, _, _)| *components)
            .map(|(_, project_id, actor)| (project_id, actor))
            .ok_or_else(|| ProjectIdentityError::UnregisteredPath(canonical).into())
    }

    /// Resolve a registered project ID to its actor without holding the registry lock.
    ///
    /// # Errors
    ///
    /// Returns [`ProjectRegistryError::ProjectNotFound`] when the ID is not registered.
    pub async fn actor_for_project(
        &self,
        id: &ProjectId,
    ) -> Result<ProjectHandle, ProjectRegistryError> {
        self.actor(id).await
    }

    /// Resolve every actor group belonging to one logical project.
    ///
    /// Compatible linked worktrees share one returned actor; incompatible
    /// worktrees remain separate actors under the same stable project ID.
    ///
    /// # Errors
    ///
    /// Returns [`ProjectRegistryError::ProjectNotFound`] when the ID is not registered.
    pub async fn actors_for_project(
        &self,
        id: &ProjectId,
    ) -> Result<Vec<ProjectHandle>, ProjectRegistryError> {
        self.projects
            .read()
            .await
            .get(id)
            .map(|project| {
                project
                    .actors
                    .iter()
                    .map(|actor| actor.actor.clone())
                    .collect()
            })
            .ok_or_else(|| ProjectRegistryError::ProjectNotFound(id.clone()))
    }

    /// Return the number of actor groups backing one logical project.
    ///
    /// Compatible linked roots share one group and therefore one language
    /// server set; incompatible roots retain separate groups.
    ///
    /// # Errors
    ///
    /// Returns [`ProjectRegistryError::ProjectNotFound`] when the ID is not
    /// registered.
    pub async fn actor_group_count(&self, id: &ProjectId) -> Result<usize, ProjectRegistryError> {
        Ok(self.actor_group_roots(id).await?.len())
    }

    /// Return the canonical roots owned by each actor group in a logical project.
    ///
    /// Compatible linked worktrees appear in one inner vector; incompatible
    /// roots appear in separate vectors. The outer order is stable for the
    /// lifetime of the registration.
    ///
    /// # Errors
    ///
    /// Returns [`ProjectRegistryError::ProjectNotFound`] when the ID is not
    /// registered.
    pub async fn actor_group_roots(
        &self,
        id: &ProjectId,
    ) -> Result<Vec<Vec<PathBuf>>, ProjectRegistryError> {
        self.projects
            .read()
            .await
            .get(id)
            .map(|project| {
                project
                    .actors
                    .iter()
                    .map(|actor| {
                        actor
                            .roots
                            .iter()
                            .map(CanonicalRoot::as_path)
                            .map(Path::to_path_buf)
                            .collect()
                    })
                    .collect()
            })
            .ok_or_else(|| ProjectRegistryError::ProjectNotFound(id.clone()))
    }

    async fn actor(&self, id: &ProjectId) -> Result<ProjectHandle, ProjectRegistryError> {
        self.projects
            .read()
            .await
            .get(id)
            .map(|project| project.primary().actor.clone())
            .ok_or_else(|| ProjectRegistryError::ProjectNotFound(id.clone()))
    }

    async fn actor_entries(
        &self,
        id: &ProjectId,
    ) -> Result<(ProjectIdentity, Vec<(ProjectHandle, MutationGate)>), ProjectRegistryError> {
        self.projects
            .read()
            .await
            .get(id)
            .map(|project| {
                (
                    project.identity.clone(),
                    project
                        .actors
                        .iter()
                        .map(|actor| (actor.actor.clone(), actor.mutation.clone()))
                        .collect(),
                )
            })
            .ok_or_else(|| ProjectRegistryError::ProjectNotFound(id.clone()))
    }

    async fn entry(
        &self,
        id: &ProjectId,
    ) -> Result<(ProjectIdentity, ProjectHandle, MutationGate), ProjectRegistryError> {
        self.projects
            .read()
            .await
            .get(id)
            .map(|project| {
                (
                    project.identity.clone(),
                    project.primary().actor.clone(),
                    project.primary().mutation.clone(),
                )
            })
            .ok_or_else(|| ProjectRegistryError::ProjectNotFound(id.clone()))
    }
}

fn aggregate_statuses(statuses: impl IntoIterator<Item = ProjectStatus>) -> ProjectStatus {
    statuses
        .into_iter()
        .max_by_key(|status| project_status_priority(*status))
        .unwrap_or(ProjectStatus::Starting)
}

fn unique_mutation_gates(projects: &HashMap<ProjectId, ProjectEntry>) -> Vec<MutationGate> {
    let mut mutations = Vec::new();
    for entry in projects.values() {
        for actor in &entry.actors {
            if !mutations
                .iter()
                .any(|existing| std::sync::Arc::ptr_eq(existing, &actor.mutation))
            {
                mutations.push(actor.mutation.clone());
            }
        }
    }
    mutations
}

fn reject_new_actor_work<'a>(actors: impl IntoIterator<Item = &'a ProjectHandle>) {
    for actor in actors {
        actor.reject_new_work();
    }
}

fn shutdown_actor_groups(
    entries: Vec<(ProjectId, ProjectHandle)>,
) -> (Vec<ProjectId>, Vec<(ProjectHandle, Vec<ProjectId>)>) {
    let mut stopped = Vec::new();
    let mut actors: Vec<(ProjectHandle, Vec<ProjectId>)> = Vec::new();
    for (id, actor) in entries {
        if matches!(*actor.status().borrow(), ProjectStatus::Stopped) {
            stopped.push(id);
            continue;
        }
        if let Some((_, project_ids)) = actors
            .iter_mut()
            .find(|(existing, _)| existing.sender.same_channel(&actor.sender))
        {
            project_ids.push(id);
        } else {
            actors.push((actor, vec![id]));
        }
    }
    (stopped, actors)
}

fn compatible_project<'a>(
    projects: &'a HashMap<ProjectId, ProjectEntry>,
    identity: &ProjectIdentity,
    compatibility_key: Option<ProjectCompatibilityKey>,
) -> Option<&'a ProjectEntry> {
    let repository = identity.repository_identity()?;
    let compatibility_key = compatibility_key?;
    projects.values().find(|project| {
        project.identity.repository_identity() == Some(repository)
            && project.has_compatible_actor(Some(compatibility_key))
    })
}

fn translator_templates_match(
    left: Option<&TranslatorTemplate>,
    right: Option<&TranslatorTemplate>,
) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(left), Some(right)) => left.same_configuration(right),
        _ => false,
    }
}

async fn save_persisted_state(
    store: std::sync::Arc<ProjectRegistrationStore>,
    projects: Vec<PersistedProject>,
) -> Result<(), ProjectRegistryError> {
    tokio::task::spawn_blocking(move || store.save(&projects))
        .await
        .map_err(|error| {
            crate::project_persistence::ProjectPersistenceError::Io(std::io::Error::other(format!(
                "persistence task failed: {error}"
            )))
        })??;
    Ok(())
}

async fn load_persisted_state(
    store: std::sync::Arc<ProjectRegistrationStore>,
) -> Result<crate::project_persistence::ProjectRegistrationState, ProjectRegistryError> {
    tokio::task::spawn_blocking(move || store.load())
        .await
        .map_err(|error| {
            crate::project_persistence::ProjectPersistenceError::Io(std::io::Error::other(format!(
                "persistence task failed: {error}"
            )))
        })?
        .map_err(ProjectRegistryError::from)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    #[tokio::test]
    async fn server_exit_forwarder_wraps_exit_in_residency_guard() {
        let controller = RustResidencyController::new(1);
        let (request_sender, mut request_receiver) = mpsc::channel(1);
        let (notification_sender, notification_receiver) = mpsc::channel(1);
        let residency = ProjectResidency {
            controller,
            group: RustGroupId(1),
        };

        let forwarder = tokio::spawn(forward_lsp_notifications(
            "rust".into(),
            notification_receiver,
            request_sender.downgrade(),
            7,
            Some(residency),
        ));
        drop(notification_sender);

        let request = tokio::time::timeout(Duration::from_secs(1), request_receiver.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            request,
            ProjectRequest::Resident { request, .. }
                if matches!(*request, ProjectRequest::ServerExited { generation: 7 })
        ));
        forwarder.await.unwrap();
    }

    #[test]
    fn code_action_store_references_are_bounded_and_single_use() {
        let mut store = CodeActionStore {
            entries: HashMap::new(),
            ttl: Duration::from_secs(60),
            max_entries: 1,
        };
        let first = store.insert(StoredCodeAction {
            file_path: "first.rs".to_string(),
            action: lsp_types::CodeActionOrCommand::Command(lsp_types::Command {
                title: "first".to_string(),
                command: "first".to_string(),
                arguments: None,
            }),
            created_at: Instant::now(),
        });
        let second = store.insert(StoredCodeAction {
            file_path: "second.rs".to_string(),
            action: lsp_types::CodeActionOrCommand::Command(lsp_types::Command {
                title: "second".to_string(),
                command: "second".to_string(),
                arguments: None,
            }),
            created_at: Instant::now(),
        });
        assert!(store.take(&first).is_err());
        assert!(store.take(&second).is_ok());
        assert!(store.take(&second).is_err());
    }

    #[test]
    fn git_identity_resolves_main_checkout_and_linked_worktree() {
        let repository = TempDir::new().unwrap();
        let git_dir = repository.path().join(".git");
        let worktree_git_dir = git_dir.join("worktrees").join("feature");
        fs::create_dir_all(&worktree_git_dir).unwrap();
        fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        fs::write(git_dir.join("config"), "[core]\n").unwrap();
        fs::create_dir(git_dir.join("objects")).unwrap();
        fs::write(worktree_git_dir.join("commondir"), "../..\n").unwrap();

        let worktree = TempDir::new().unwrap();
        fs::write(
            worktree.path().join(".git"),
            format!("gitdir: {}\n", worktree_git_dir.display()),
        )
        .unwrap();

        let main_identity = GitRepositoryIdentity::discover(repository.path())
            .unwrap()
            .unwrap();
        let worktree_identity = GitRepositoryIdentity::discover(worktree.path())
            .unwrap()
            .unwrap();

        assert_eq!(main_identity.common_dir(), git_dir.canonicalize().unwrap());
        assert_eq!(worktree_identity, main_identity);
    }

    #[test]
    fn git_identity_distinguishes_non_git_and_stale_metadata() {
        let plain = TempDir::new().unwrap();
        assert!(
            GitRepositoryIdentity::discover(plain.path())
                .unwrap()
                .is_none()
        );

        let stale = TempDir::new().unwrap();
        fs::write(stale.path().join(".git"), "gitdir: /missing/worktree\n").unwrap();
        assert!(matches!(
            GitRepositoryIdentity::discover(stale.path()),
            Err(GitRepositoryIdentityError::MissingGitDirectory { .. })
        ));
    }

    #[tokio::test]
    async fn rust_compatibility_key_changes_with_server_configuration() {
        let root = TempDir::new().unwrap();
        fs::write(
            root.path().join("rust-toolchain.toml"),
            "[toolchain]\nchannel = \"stable\"\n",
        )
        .unwrap();
        fs::write(
            root.path().join("Cargo.toml"),
            "[package]\nname = \"fixture\"\n",
        )
        .unwrap();

        let mut first = Translator::new();
        first.set_lsp_configs(
            vec![crate::config::LspServerConfig::rust_analyzer()],
            Some(10),
        );
        let mut changed_config = crate::config::LspServerConfig::rust_analyzer();
        changed_config.args.push("--log-file=ra.log".to_string());
        let mut second = Translator::new();
        second.set_lsp_configs(vec![changed_config], Some(10));

        assert_ne!(
            rust_project_compatibility_key(root.path(), Some(&first.configuration_template()))
                .await,
            rust_project_compatibility_key(root.path(), Some(&second.configuration_template()))
                .await,
        );
    }

    #[tokio::test]
    async fn rust_compatibility_key_changes_with_edit_safety_policy() {
        let root = TempDir::new().unwrap();
        fs::write(
            root.path().join("rust-toolchain.toml"),
            "[toolchain]\nchannel = \"stable\"\n",
        )
        .unwrap();
        fs::write(
            root.path().join("Cargo.toml"),
            "[package]\nname = \"fixture\"\n",
        )
        .unwrap();

        let mut translator = Translator::new();
        translator.set_lsp_configs(
            vec![crate::config::LspServerConfig::rust_analyzer()],
            Some(10),
        );
        let first = translator.configuration_template();
        let second = first.clone().with_project_config(&ProjectConfig {
            edit_safety: Some(EditSafetyConfig {
                audit_log: Some(crate::config::AuditLogConfig {
                    path: PathBuf::from("audit.jsonl"),
                    max_bytes: 4_096,
                    failure_mode: crate::edit_plan::AuditFailureMode::FailClosed,
                }),
                backup: None,
            }),
            ..ProjectConfig::default()
        });

        assert_ne!(
            rust_project_compatibility_key(root.path(), Some(&first)).await,
            rust_project_compatibility_key(root.path(), Some(&second)).await
        );
    }

    #[tokio::test]
    async fn rust_compatibility_key_changes_with_file_patterns() {
        let root = TempDir::new().unwrap();
        fs::write(
            root.path().join("rust-toolchain.toml"),
            "[toolchain]\nchannel = \"stable\"\n",
        )
        .unwrap();
        fs::write(
            root.path().join("Cargo.toml"),
            "[package]\nname = \"fixture\"\n",
        )
        .unwrap();

        let mut first_config = crate::config::LspServerConfig::rust_analyzer();
        first_config.file_patterns = vec!["**/*.rs".to_string()];
        let mut second_config = first_config.clone();
        second_config.file_patterns = vec!["**/*.rs", "**/*.toml"]
            .into_iter()
            .map(str::to_string)
            .collect();
        let mut first = Translator::new();
        first.set_lsp_configs(vec![first_config], Some(10));
        let mut second = Translator::new();
        second.set_lsp_configs(vec![second_config], Some(10));

        assert_ne!(
            rust_project_compatibility_key(root.path(), Some(&first.configuration_template()))
                .await,
            rust_project_compatibility_key(root.path(), Some(&second.configuration_template()))
                .await,
        );
    }

    #[tokio::test]
    async fn rust_compatibility_key_ignores_manifest_and_lockfile_contents() {
        let first = TempDir::new().unwrap();
        let second = TempDir::new().unwrap();
        for root in [first.path(), second.path()] {
            fs::write(
                root.join("rust-toolchain.toml"),
                "[toolchain]\nchannel = \"stable\"\n",
            )
            .unwrap();
        }
        fs::write(
            first.path().join("Cargo.toml"),
            "[package]\nname = \"first\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        fs::write(
            second.path().join("Cargo.toml"),
            "[package]\nname = \"second\"\nversion = \"0.2.0\"\n",
        )
        .unwrap();
        fs::write(first.path().join("Cargo.lock"), "version = 3\n").unwrap();
        fs::write(
            second.path().join("Cargo.lock"),
            "version = 4\n\n[[package]]\nname = \"dependency\"\nversion = \"1.0.0\"\n",
        )
        .unwrap();
        let mut translator = Translator::new();
        translator.set_lsp_configs(
            vec![crate::config::LspServerConfig::rust_analyzer()],
            Some(10),
        );
        let template = translator.configuration_template();

        assert_eq!(
            rust_project_compatibility_key(first.path(), Some(&template)).await,
            rust_project_compatibility_key(second.path(), Some(&template)).await
        );
    }

    #[tokio::test]
    async fn rust_compatibility_key_rejects_dynamic_project_environment() {
        let root = TempDir::new().unwrap();
        fs::write(
            root.path().join("rust-toolchain.toml"),
            "[toolchain]\nchannel = \"stable\"\n",
        )
        .unwrap();
        fs::write(
            root.path().join("Cargo.toml"),
            "[package]\nname = \"fixture\"\n",
        )
        .unwrap();
        fs::write(
            root.path().join(".envrc"),
            "export RUSTFLAGS=-Ctarget-cpu=native\n",
        )
        .unwrap();

        let mut translator = Translator::new();
        translator.set_lsp_configs(
            vec![crate::config::LspServerConfig::rust_analyzer()],
            Some(10),
        );

        assert_eq!(
            rust_project_compatibility_key(root.path(), Some(&translator.configuration_template()))
                .await,
            None
        );

        fs::remove_file(root.path().join(".envrc")).unwrap();
        fs::write(root.path().join("flake.nix"), "{}\n").unwrap();
        assert_eq!(
            rust_project_compatibility_key(root.path(), Some(&translator.configuration_template()))
                .await,
            None
        );
    }

    #[test]
    fn rust_compatibility_environment_ignores_ephemeral_values() {
        let mut first = HashMap::from([
            ("DIRENV_DIFF".to_string(), Some("first".to_string())),
            ("PWD".to_string(), Some("/first".to_string())),
            (
                "RUSTFLAGS".to_string(),
                Some("-Ctarget-cpu=native".to_string()),
            ),
        ]);
        let mut second = first.clone();
        second.insert("DIRENV_DIFF".to_string(), Some("second".to_string()));
        second.insert("PWD".to_string(), Some("/second".to_string()));

        let fingerprint = |environment: &HashMap<String, Option<String>>| {
            let mut hasher = Sha256::new();
            hash_project_environment(&mut hasher, Some(environment));
            hasher.finalize()
        };

        assert_eq!(fingerprint(&first), fingerprint(&second));
        first.insert(
            "RUSTFLAGS".to_string(),
            Some("-Ctarget-cpu=generic".to_string()),
        );
        assert_ne!(fingerprint(&first), fingerprint(&second));
    }

    #[tokio::test]
    async fn rust_compatibility_key_rejects_unavailable_toolchains() {
        let root = TempDir::new().unwrap();
        fs::write(
            root.path().join("rust-toolchain.toml"),
            "[toolchain]\nchannel = \"mcpls-definitely-missing\"\n",
        )
        .unwrap();
        fs::write(
            root.path().join("Cargo.toml"),
            "[package]\nname = \"fixture\"\n",
        )
        .unwrap();

        let mut translator = Translator::new();
        translator.set_lsp_configs(
            vec![crate::config::LspServerConfig::rust_analyzer()],
            Some(10),
        );

        assert_eq!(
            rust_project_compatibility_key(root.path(), Some(&translator.configuration_template()))
                .await,
            None
        );
    }

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

    #[test]
    fn resolve_rejects_explicit_id_and_path_mismatch() {
        let first = TempDir::new().unwrap();
        let second = TempDir::new().unwrap();
        let file = second.path().join("src.rs");
        fs::write(&file, "fn main() {}").unwrap();
        let first_id = ProjectId::new("first").unwrap();
        let projects = [
            ProjectIdentity::new(first_id.clone(), CanonicalRoot::new(first.path()).unwrap()),
            ProjectIdentity::new(
                ProjectId::new("second").unwrap(),
                CanonicalRoot::new(second.path()).unwrap(),
            ),
        ];
        let project_resolver = ProjectResolver::new(projects).unwrap();

        assert!(matches!(
            project_resolver.resolve(Some(&first_id), Some(&file)),
            Err(ProjectIdentityError::ProjectPathMismatch { id, .. }) if id == first_id
        ));
    }

    #[test]
    fn longest_matching_root_uses_path_components() {
        let roots = vec![
            PathBuf::from("/workspace/project"),
            PathBuf::from("/workspace/project/nested"),
            PathBuf::from("/workspace/project-other"),
        ];

        let root = longest_matching_root(Path::new("/workspace/project/nested/src.rs"), &roots);

        assert_eq!(root, Some(Path::new("/workspace/project/nested")));
    }

    #[test]
    fn resolve_path_reports_deleted_project_root() {
        let workspace = TempDir::new().unwrap();
        let root = workspace.path().to_path_buf();
        let file = root.join("src.rs");
        fs::write(&file, "fn main() {}").unwrap();
        let project = ProjectIdentity::new(
            ProjectId::new("deleted").unwrap(),
            CanonicalRoot::new(&root).unwrap(),
        );
        let project_resolver = ProjectResolver::new([project]).unwrap();
        fs::remove_dir_all(&root).unwrap();

        assert!(matches!(
            project_resolver.resolve_path(&file),
            Err(ProjectIdentityError::ProjectRootUnavailable(id)) if id.as_str() == "deleted"
        ));
    }

    #[tokio::test]
    async fn project_actor_reports_status_transitions() {
        let handle = spawn_project_actor(4);

        assert_eq!(handle.status().borrow().clone(), ProjectStatus::Starting);
        handle.set_status(ProjectStatus::Ready).await.unwrap();

        assert_eq!(handle.status().borrow().clone(), ProjectStatus::Ready);
    }

    #[tokio::test]
    async fn project_actor_skips_cancelled_queued_mutation() {
        let actor = spawn_project_actor(2);
        let (reply, response) = oneshot::channel();
        drop(response);
        actor
            .sender
            .send(ProjectRequest::SetStatus {
                status: ProjectStatus::Ready,
                reply,
            })
            .await
            .unwrap();

        assert_eq!(
            actor.query().await.unwrap().status(),
            ProjectStatus::Starting
        );
    }

    #[tokio::test]
    async fn project_actor_delivers_active_mutation_after_response_cancellation() {
        let (status_tx, _) = watch::channel(ProjectStatus::Starting);
        let (event_tx, _) = broadcast::channel(1);
        let channels = ProjectActorChannels {
            status_tx,
            event_tx,
            event_history: std::sync::Arc::new(std::sync::Mutex::new(ProjectEventHistory::new(1))),
        };
        let (sender, _receiver) = mpsc::channel(1);
        let actor_sender = sender.downgrade();
        let mut runtime = ProjectRuntime::new(Translator::new());
        let mut state = ProjectState::new(ProjectStatus::Starting, runtime.summary());
        let (reply, response) = oneshot::channel();
        drop(response);

        assert!(
            !handle_project_request(
                ProjectRequest::SetStatus {
                    status: ProjectStatus::Ready,
                    reply,
                },
                &actor_sender,
                &channels,
                &mut state,
                &mut runtime,
                None,
            )
            .await
        );
        assert_eq!(state.status(), ProjectStatus::Ready);
    }

    #[tokio::test]
    async fn project_request_waiting_on_full_queue_is_rejected_when_work_closes() {
        let (sender, mut receiver) = mpsc::channel(1);
        let sender = ProjectRequestSender::new(sender);
        sender
            .send(ProjectRequest::ServerExited { generation: 0 })
            .await
            .unwrap();

        let (reply, _response) = oneshot::channel();
        let mut pending = tokio::spawn({
            let sender = sender.clone();
            async move {
                sender
                    .send(ProjectRequest::SetStatus {
                        status: ProjectStatus::Ready,
                        reply,
                    })
                    .await
            }
        });

        assert!(
            tokio::time::timeout(Duration::from_millis(10), &mut pending)
                .await
                .is_err()
        );
        sender.reject_new_work();
        let _ = receiver.recv().await;

        let result = tokio::time::timeout(Duration::from_secs(1), pending)
            .await
            .unwrap()
            .unwrap();
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn queued_resident_request_pins_group_before_actor_dequeues_it() {
        let controller = RustResidencyController::new(1);
        let (first_channel, mut first_receiver) = mpsc::channel(4);
        let (second_channel, mut second_receiver) = mpsc::channel(4);
        let first_residency = ProjectResidency {
            controller: controller.clone(),
            group: RustGroupId(1),
        };
        let second_residency = ProjectResidency {
            controller: controller.clone(),
            group: RustGroupId(2),
        };
        controller.register(RustGroupId(1), first_channel.downgrade());
        controller.register(RustGroupId(2), second_channel.downgrade());
        let first_sender = ProjectRequestSender::with_residency(first_channel, first_residency);
        let second_sender = ProjectRequestSender::with_residency(second_channel, second_residency);

        let (first_reply, _first_response) = oneshot::channel();
        first_sender
            .send(ProjectRequest::Activate {
                root: PathBuf::from("first"),
                reply: first_reply,
            })
            .await
            .unwrap();

        let (second_reply, _second_response) = oneshot::channel();
        let mut second_send = tokio::spawn(async move {
            second_sender
                .send(ProjectRequest::Activate {
                    root: PathBuf::from("second"),
                    reply: second_reply,
                })
                .await
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut second_send)
                .await
                .is_err()
        );

        let first_request = first_receiver.recv().await.unwrap();
        assert!(matches!(first_request, ProjectRequest::Resident { .. }));
        drop(first_request);
        let suspend = tokio::time::timeout(Duration::from_secs(1), first_receiver.recv())
            .await
            .unwrap()
            .unwrap();
        let ProjectRequest::Suspend { reply } = suspend else {
            panic!("expected eviction only after the queued request completed");
        };
        reply.send(Ok(())).unwrap();

        second_send.await.unwrap().unwrap();
        assert!(matches!(
            second_receiver.recv().await.unwrap(),
            ProjectRequest::Resident { .. }
        ));
    }

    #[tokio::test]
    async fn project_actor_publishes_typed_status_events() {
        let handle = spawn_project_actor(4);
        let mut events = handle.subscribe_events();

        handle.set_status(ProjectStatus::Ready).await.unwrap();

        assert_eq!(
            events.recv().await.unwrap(),
            ProjectEvent::StatusChanged {
                status: ProjectStatus::Ready,
                last_error: None,
            }
        );
    }

    #[test]
    fn project_event_history_bounds_records_and_reports_cursor_resync() {
        let mut history = ProjectEventHistory::new(2);
        history.record(ProjectEvent::StatusChanged {
            status: ProjectStatus::Starting,
            last_error: None,
        });
        history.record(ProjectEvent::StatusChanged {
            status: ProjectStatus::Ready,
            last_error: None,
        });
        history.record(ProjectEvent::ServerExited { generation: 1 });

        let snapshot = history.snapshot_since(Some(0));
        assert!(snapshot.resync_required());
        assert_eq!(snapshot.events().len(), 2);
        assert_eq!(snapshot.events()[0].sequence(), 2);
        assert_eq!(snapshot.events()[1].sequence(), 3);
    }

    #[test]
    fn project_event_history_retains_edit_completion_and_file_change_payloads() {
        let mut history = ProjectEventHistory::new(4);
        let plan_id = PlanId::parse("plan-1").unwrap();
        history.record(ProjectEvent::FilesChanged {
            paths: vec![PathBuf::from("/workspace/main.rs")],
        });
        history.record(ProjectEvent::EditApplied {
            plan_id: plan_id.clone(),
            committed_files: vec![PathBuf::from("/workspace/main.rs")],
            operation_count: 1,
        });

        let snapshot = history.snapshot_since(None);
        assert!(matches!(
            snapshot.events()[0].event(),
            ProjectEvent::FilesChanged { paths } if paths.len() == 1
        ));
        assert!(matches!(
            snapshot.events()[1].event(),
            ProjectEvent::EditApplied {
                plan_id: actual,
                operation_count: 1,
                ..
            } if actual == &plan_id
        ));
        assert_eq!(
            snapshot.events()[0].event().json_value(),
            serde_json::json!({
                "kind": "files_changed",
                "paths": ["/workspace/main.rs"],
            })
        );
        assert_eq!(
            snapshot.events()[1].event().json_value(),
            serde_json::json!({
                "kind": "edit_applied",
                "plan_id": "plan-1",
                "committed_files": ["/workspace/main.rs"],
                "operation_count": 1,
            })
        );
    }

    #[tokio::test]
    async fn project_actor_publishes_server_exit_events_before_recovery_status() {
        let handle = spawn_project_actor(4);
        let mut events = handle.subscribe_events();
        handle.set_status(ProjectStatus::Ready).await.unwrap();
        let _ = events.recv().await.unwrap();

        handle
            .sender
            .send(ProjectRequest::ServerExited { generation: 0 })
            .await
            .unwrap();

        assert_eq!(
            events.recv().await.unwrap(),
            ProjectEvent::ServerExited { generation: 0 }
        );
        assert_eq!(
            events.recv().await.unwrap(),
            ProjectEvent::StatusChanged {
                status: ProjectStatus::Restarting,
                last_error: Some("language server exited; restarting (attempt 1/3)".to_string(),),
            }
        );
        assert_eq!(
            events.recv().await.unwrap(),
            ProjectEvent::StatusChanged {
                status: ProjectStatus::Ready,
                last_error: None,
            }
        );
    }

    #[tokio::test]
    async fn project_actor_shutdown_publishes_stopped_and_closes_requests() {
        let handle = spawn_project_actor(1);

        handle.shutdown().await.unwrap();

        assert_eq!(handle.status().borrow().clone(), ProjectStatus::Stopped);
        assert!(matches!(
            handle.set_status(ProjectStatus::Ready).await,
            Err(ProjectActorError::Closed)
        ));
    }

    #[tokio::test]
    async fn dropping_last_project_handle_stops_actor() {
        let handle = spawn_project_actor(1);
        let mut status = handle.status();

        drop(handle);

        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(1), async {
                while *status.borrow() != ProjectStatus::Stopped {
                    status.changed().await.unwrap();
                }
            })
            .await
            .is_ok(),
            "actor did not stop after its last handle was dropped"
        );
    }

    #[tokio::test]
    async fn project_actor_exposes_typed_query_refresh_restart_and_failure() {
        let handle = spawn_project_actor(4);

        assert_eq!(
            handle.query().await.unwrap().status(),
            ProjectStatus::Starting
        );
        assert_eq!(
            handle.refresh().await.unwrap().status(),
            ProjectStatus::Starting
        );
        assert_eq!(
            handle.restart().await.unwrap().status(),
            ProjectStatus::Ready
        );

        handle.fail("rust-analyzer exited").await.unwrap();
        let state = handle.query().await.unwrap();
        assert_eq!(state.status(), ProjectStatus::Failed);
        assert_eq!(state.last_error(), Some("rust-analyzer exited"));
    }

    #[tokio::test]
    async fn project_actor_owns_project_workspace_state() {
        let root = TempDir::new().unwrap();
        let canonical_root = CanonicalRoot::new(root.path()).unwrap();
        let handle = spawn_project_actor_for_root(2, &canonical_root);

        let state = handle.query().await.unwrap();

        assert_eq!(
            state.workspace_roots(),
            &[root.path().canonicalize().unwrap()]
        );
        assert_eq!(state.open_document_count(), 0);
    }

    #[tokio::test]
    async fn project_actor_routes_semantic_requests_through_owned_translator() {
        let root = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        fs::write(outside.path().join("outside.rs"), "fn outside() {}\n").unwrap();
        let canonical_root = CanonicalRoot::new(root.path()).unwrap();
        let handle = spawn_project_actor_for_root(2, &canonical_root);

        let result = handle
            .hover(
                outside.path().join("outside.rs").display().to_string(),
                0,
                0,
            )
            .await;

        assert!(matches!(
            result,
            Err(ProjectActorError::Operation(message)) if message.contains("outside workspace")
        ));
    }

    #[tokio::test]
    async fn project_actor_routes_definition_requests_through_owned_translator() {
        let root = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let file = outside.path().join("outside.rs");
        fs::write(&file, "fn outside() {}\n").unwrap();
        let canonical_root = CanonicalRoot::new(root.path()).unwrap();
        let handle = spawn_project_actor_for_root(2, &canonical_root);

        let result = handle.definition(file.display().to_string(), 0, 0).await;

        assert!(matches!(
            result,
            Err(ProjectActorError::Operation(message)) if message.contains("outside workspace")
        ));
    }

    #[tokio::test]
    async fn project_actor_routes_references_requests_through_owned_translator() {
        let root = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let file = outside.path().join("outside.rs");
        fs::write(&file, "fn outside() {}\n").unwrap();
        let canonical_root = CanonicalRoot::new(root.path()).unwrap();
        let handle = spawn_project_actor_for_root(2, &canonical_root);

        let result = handle
            .references(file.display().to_string(), 0, 0, false)
            .await;

        assert!(matches!(
            result,
            Err(ProjectActorError::Operation(message)) if message.contains("outside workspace")
        ));
    }

    #[tokio::test]
    async fn project_actor_routes_diagnostics_requests_through_owned_translator() {
        let root = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let file = outside.path().join("outside.rs");
        fs::write(&file, "fn outside() {}\n").unwrap();
        let canonical_root = CanonicalRoot::new(root.path()).unwrap();
        let handle = spawn_project_actor_for_root(2, &canonical_root);

        let result = handle.diagnostics(file.display().to_string()).await;

        assert!(matches!(
            result,
            Err(ProjectActorError::Operation(message)) if message.contains("outside workspace")
        ));
    }

    #[tokio::test]
    async fn project_actor_routes_rename_requests_through_owned_translator() {
        let root = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let file = outside.path().join("outside.rs");
        fs::write(&file, "fn outside() {}\n").unwrap();
        let canonical_root = CanonicalRoot::new(root.path()).unwrap();
        let handle = spawn_project_actor_for_root(2, &canonical_root);

        let result = handle
            .rename(file.display().to_string(), 0, 0, "renamed".to_string())
            .await;

        assert!(matches!(
            result,
            Err(ProjectActorError::Operation(message)) if message.contains("outside workspace")
        ));
    }

    #[tokio::test]
    async fn project_actor_routes_raw_rename_edits_through_owned_translator() {
        let root = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let file = outside.path().join("outside.rs");
        fs::write(&file, "fn outside() {}\n").unwrap();
        let canonical_root = CanonicalRoot::new(root.path()).unwrap();
        let handle = spawn_project_actor_for_root(2, &canonical_root);

        let result = handle
            .rename_workspace_edit(file.display().to_string(), 0, 0, "renamed".to_string())
            .await;

        assert!(matches!(
            result,
            Err(ProjectActorError::Operation(message)) if message.contains("outside workspace")
        ));
    }

    #[tokio::test]
    async fn project_actor_routes_completion_requests_through_owned_translator() {
        let root = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let file = outside.path().join("outside.rs");
        fs::write(&file, "fn outside() {}\n").unwrap();
        let canonical_root = CanonicalRoot::new(root.path()).unwrap();
        let handle = spawn_project_actor_for_root(2, &canonical_root);

        let result = handle
            .completions(file.display().to_string(), 0, 0, None)
            .await;

        assert!(matches!(
            result,
            Err(ProjectActorError::Operation(message)) if message.contains("outside workspace")
        ));
    }

    #[tokio::test]
    async fn project_actor_routes_document_symbol_requests_through_owned_translator() {
        let root = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let file = outside.path().join("outside.rs");
        fs::write(&file, "fn outside() {}\n").unwrap();
        let canonical_root = CanonicalRoot::new(root.path()).unwrap();
        let handle = spawn_project_actor_for_root(2, &canonical_root);

        let result = handle.document_symbols(file.display().to_string()).await;

        assert!(matches!(
            result,
            Err(ProjectActorError::Operation(message)) if message.contains("outside workspace")
        ));
    }

    #[tokio::test]
    async fn project_actor_routes_format_requests_through_owned_translator() {
        let root = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let file = outside.path().join("outside.rs");
        fs::write(&file, "fn outside() {}\n").unwrap();
        let canonical_root = CanonicalRoot::new(root.path()).unwrap();
        let handle = spawn_project_actor_for_root(2, &canonical_root);

        let result = handle
            .format_document(file.display().to_string(), 4, true)
            .await;

        assert!(matches!(
            result,
            Err(ProjectActorError::Operation(message)) if message.contains("outside workspace")
        ));
    }

    #[tokio::test]
    async fn project_actor_routes_raw_format_edits_through_owned_translator() {
        let root = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let file = outside.path().join("outside.rs");
        fs::write(&file, "fn outside() {}\n").unwrap();
        let canonical_root = CanonicalRoot::new(root.path()).unwrap();
        let handle = spawn_project_actor_for_root(2, &canonical_root);

        let result = handle
            .format_workspace_edit(file.display().to_string(), 4, true)
            .await;

        assert!(matches!(
            result,
            Err(ProjectActorError::Operation(message)) if message.contains("outside workspace")
        ));
    }

    #[tokio::test]
    async fn project_actor_routes_code_action_requests_through_owned_translator() {
        let root = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let file = outside.path().join("outside.rs");
        fs::write(&file, "fn outside() {}\n").unwrap();
        let canonical_root = CanonicalRoot::new(root.path()).unwrap();
        let handle = spawn_project_actor_for_root(2, &canonical_root);

        let result = handle
            .code_actions(file.display().to_string(), 1, 5, 1, 15, None)
            .await;

        assert!(matches!(
            result,
            Err(ProjectActorError::Operation(message)) if message.contains("outside workspace")
        ));
    }

    #[tokio::test]
    async fn project_actor_routes_call_hierarchy_requests_through_owned_translator() {
        let root = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let file = outside.path().join("outside.rs");
        fs::write(&file, "fn outside() {}\n").unwrap();
        let canonical_root = CanonicalRoot::new(root.path()).unwrap();
        let handle = spawn_project_actor_for_root(2, &canonical_root);

        let result = handle
            .prepare_call_hierarchy(file.display().to_string(), 1, 5)
            .await;

        assert!(matches!(
            result,
            Err(ProjectActorError::Operation(message)) if message.contains("outside workspace")
        ));
    }

    #[tokio::test]
    async fn project_actor_routes_signature_help_through_owned_translator() {
        let root = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let file = outside.path().join("outside.rs");
        fs::write(&file, "fn outside() {}\n").unwrap();
        let canonical_root = CanonicalRoot::new(root.path()).unwrap();
        let handle = spawn_project_actor_for_root(2, &canonical_root);

        let result = handle
            .signature_help(file.display().to_string(), 1, 5)
            .await;

        assert!(matches!(
            result,
            Err(ProjectActorError::Operation(message)) if message.contains("outside workspace")
        ));
    }

    #[tokio::test]
    async fn project_actor_routes_inlay_hints_through_owned_translator() {
        let root = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let file = outside.path().join("outside.rs");
        fs::write(&file, "fn outside() {}\n").unwrap();
        let canonical_root = CanonicalRoot::new(root.path()).unwrap();
        let handle = spawn_project_actor_for_root(2, &canonical_root);

        let result = handle
            .inlay_hints(file.display().to_string(), 1, 5, 1, 15)
            .await;

        assert!(matches!(
            result,
            Err(ProjectActorError::Operation(message)) if message.contains("outside workspace")
        ));
    }

    #[tokio::test]
    async fn project_actor_routes_implementation_requests_through_owned_translator() {
        let root = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let file = outside.path().join("outside.rs");
        fs::write(&file, "fn outside() {}\n").unwrap();
        let canonical_root = CanonicalRoot::new(root.path()).unwrap();
        let handle = spawn_project_actor_for_root(2, &canonical_root);

        let result = handle
            .go_to_implementation(file.display().to_string(), 1, 5)
            .await;

        assert!(matches!(
            result,
            Err(ProjectActorError::Operation(message)) if message.contains("outside workspace")
        ));
    }

    #[tokio::test]
    async fn project_actor_routes_type_definition_requests_through_owned_translator() {
        let root = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let file = outside.path().join("outside.rs");
        fs::write(&file, "fn outside() {}\n").unwrap();
        let canonical_root = CanonicalRoot::new(root.path()).unwrap();
        let handle = spawn_project_actor_for_root(2, &canonical_root);

        let result = handle
            .go_to_type_definition(file.display().to_string(), 1, 5)
            .await;

        assert!(matches!(
            result,
            Err(ProjectActorError::Operation(message)) if message.contains("outside workspace")
        ));
    }

    #[tokio::test]
    async fn project_actor_routes_cached_diagnostics_through_owned_translator() {
        let root = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let file = outside.path().join("outside.rs");
        fs::write(&file, "fn outside() {}\n").unwrap();
        let canonical_root = CanonicalRoot::new(root.path()).unwrap();
        let handle = spawn_project_actor_for_root(2, &canonical_root);

        let result = handle.cached_diagnostics(file.display().to_string()).await;

        assert!(matches!(
            result,
            Err(ProjectActorError::Operation(message)) if message.contains("outside workspace")
        ));
    }

    #[tokio::test]
    async fn project_actor_owns_notification_cache_for_server_logs() {
        let actor = spawn_project_actor(2);
        let notification = LspNotification::parse(
            "window/logMessage",
            Some(serde_json::json!({"type": 3, "message": "project log"})),
        );
        actor
            .sender
            .send(ProjectRequest::Notification {
                generation: 0,
                server_id: ServerId::from("rust"),
                notification,
            })
            .await
            .unwrap();

        let result = actor.server_logs(10, None).await.unwrap();
        assert_eq!(result.logs.len(), 1);
        assert_eq!(result.logs[0].message, "project log");
        assert_eq!(result.logs[0].generation, 0);
    }

    #[tokio::test]
    async fn project_actor_publishes_diagnostics_events_for_notifications() {
        let actor = spawn_project_actor(2);
        let mut events = actor.subscribe_events();
        let uri = "file:///project/src/main.rs";
        let notification = LspNotification::parse(
            "textDocument/publishDiagnostics",
            Some(serde_json::json!({
                "uri": uri,
                "version": 7,
                "diagnostics": []
            })),
        );

        actor
            .sender
            .send(ProjectRequest::Notification {
                generation: 99,
                server_id: ServerId::from("rust"),
                notification: LspNotification::parse(
                    "textDocument/publishDiagnostics",
                    Some(serde_json::json!({
                        "uri": "file:///project/src/stale.rs",
                        "diagnostics": []
                    })),
                ),
            })
            .await
            .unwrap();
        actor
            .sender
            .send(ProjectRequest::Notification {
                generation: 0,
                server_id: ServerId::from("rust"),
                notification,
            })
            .await
            .unwrap();

        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(1), events.recv())
                .await
                .unwrap()
                .unwrap(),
            ProjectEvent::DiagnosticsUpdated {
                uri: uri.to_string(),
                version: Some(7),
                diagnostic_count: 0,
            }
        );
    }

    #[tokio::test]
    async fn project_actor_reports_cached_diagnostics_presence_after_notification() {
        let root = TempDir::new().unwrap();
        let file = root.path().join("src.rs");
        fs::write(&file, "fn main() {}\n").unwrap();
        let actor = spawn_project_actor_for_root(2, &CanonicalRoot::new(root.path()).unwrap());
        let uri = crate::bridge::path_to_uri(&file).unwrap();
        actor
            .sender
            .send(ProjectRequest::Notification {
                generation: 0,
                server_id: ServerId::from("rust"),
                notification: LspNotification::parse(
                    "textDocument/publishDiagnostics",
                    Some(serde_json::json!({
                        "uri": uri,
                        "diagnostics": []
                    })),
                ),
            })
            .await
            .unwrap();

        assert!(
            actor
                .has_cached_diagnostics(file.display().to_string())
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn project_actor_becomes_ready_when_initial_rust_indexing_finishes() {
        let translator = Translator::new();
        translator.set_expected_languages(HashSet::from(["rust".to_string()]));
        let actor = spawn_project_actor_with_translator(2, translator);

        actor
            .sender
            .send(ProjectRequest::Notification {
                generation: 0,
                server_id: ServerId::from("rust"),
                notification: LspNotification::parse(
                    "experimental/serverStatus",
                    Some(serde_json::json!({
                        "health": "ok",
                        "quiescent": false
                    })),
                ),
            })
            .await
            .unwrap();
        assert_eq!(
            actor.query().await.unwrap().status(),
            ProjectStatus::Starting
        );

        actor
            .sender
            .send(ProjectRequest::Notification {
                generation: 0,
                server_id: ServerId::from("rust"),
                notification: LspNotification::parse(
                    "$/progress",
                    Some(serde_json::json!({
                        "token": "rustAnalyzer/Indexing",
                        "value": {"kind": "end"}
                    })),
                ),
            })
            .await
            .unwrap();

        assert_eq!(actor.query().await.unwrap().status(), ProjectStatus::Ready);
    }

    #[tokio::test]
    async fn repeated_activation_preserves_active_runtime_generation() {
        let root = TempDir::new().unwrap();
        let root = root.path().to_path_buf();
        let mut translator = Translator::new();
        let mut config = crate::config::LspServerConfig::rust_analyzer();
        config.heuristics = None;
        translator.set_workspace_roots(vec![root.clone()]);
        translator.set_lsp_configs(vec![config.clone()], None);
        translator.register_client(
            config.language_id.clone(),
            crate::lsp::LspClient::new(config),
        );
        translator.register_server_roots("rust".to_string(), vec![root.clone()]);

        let actor = spawn_project_actor_with_translator(2, translator);
        actor.set_status(ProjectStatus::Ready).await.unwrap();
        assert_eq!(actor.query().await.unwrap().runtime().generation(), 0);

        actor.activate(root).await.unwrap();

        assert_eq!(actor.query().await.unwrap().runtime().generation(), 0);
    }

    #[cfg(unix)]
    const DUPLICATE_ACTIVATION_LSP: &str = r#"#!/usr/bin/env python3
import json
import os
import pathlib
import sys

counter = pathlib.Path(os.environ["MCPLS_SPAWN_COUNTER"])
value = int(counter.read_text()) if counter.exists() else 0
counter.write_text(str(value + 1))

def read_message():
    headers = b""
    while b"\r\n\r\n" not in headers:
        chunk = sys.stdin.buffer.read(1)
        if not chunk:
            return None
        headers += chunk
    length = next(
        int(line.split(b":", 1)[1].strip())
        for line in headers.split(b"\r\n")
        if line.lower().startswith(b"content-length:")
    )
    return json.loads(sys.stdin.buffer.read(length))

def send(message):
    body = json.dumps(message, separators=(",", ":")).encode()
    sys.stdout.buffer.write(
        b"Content-Length: " + str(len(body)).encode() + b"\r\n\r\n" + body
    )
    sys.stdout.buffer.flush()

while True:
    message = read_message()
    if message is None:
        break
    if message.get("method") == "initialize":
        send({"jsonrpc": "2.0", "id": message["id"], "result": {
            "capabilities": {"positionEncoding": "utf-8"}
        }})
        send({"jsonrpc": "2.0", "method": "experimental/serverStatus",
              "params": {"health": "ok", "quiescent": True}})
    elif message.get("method") == "shutdown":
        send({"jsonrpc": "2.0", "id": message["id"], "result": None})
        break
"#;

    fn write_compatible_roots_with_changed_manifests(first: &Path, second: &Path) {
        for root in [first, second] {
            fs::write(
                root.join("rust-toolchain.toml"),
                "[toolchain]\nchannel = \"stable\"\n",
            )
            .unwrap();
        }
        fs::write(
            first.join("Cargo.toml"),
            "[package]\nname = \"fixture-main\"\n",
        )
        .unwrap();
        fs::write(
            second.join("Cargo.toml"),
            "[package]\nname = \"fixture-linked\"\n",
        )
        .unwrap();
        fs::write(first.join("Cargo.lock"), "version = 3\n").unwrap();
        fs::write(
            second.join("Cargo.lock"),
            "version = 4\n\n[[package]]\nname = \"changed\"\nversion = \"1.0.0\"\n",
        )
        .unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn repeated_activation_does_not_spawn_a_duplicate_lsp_process() {
        use std::collections::HashMap;
        use std::os::unix::fs::PermissionsExt;

        let root = TempDir::new().unwrap();
        let counter = root.path().join("spawn-count");
        let lsp = root.path().join("counting-lsp.py");
        fs::write(&lsp, DUPLICATE_ACTIVATION_LSP).unwrap();
        let mut permissions = fs::metadata(&lsp).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&lsp, permissions).unwrap();

        let mut config = crate::config::LspServerConfig::rust_analyzer();
        config.command = lsp.display().to_string();
        config.heuristics = None;
        config.env = HashMap::from([(
            "MCPLS_SPAWN_COUNTER".to_string(),
            counter.display().to_string(),
        )]);
        let mut translator = Translator::new()
            .with_extensions(HashMap::from([("rs".to_string(), "rust".to_string())]));
        translator.set_workspace_roots(vec![root.path().to_path_buf()]);
        translator.set_lsp_configs(vec![config], Some(3));
        let actor = spawn_project_actor_with_translator(2, translator);

        let first = actor.activate(root.path().to_path_buf()).await.unwrap();
        assert_eq!(first.status(), ProjectStatus::Starting);
        assert_eq!(fs::read_to_string(&counter).unwrap(), "1");

        let second = actor.activate(root.path().to_path_buf()).await.unwrap();
        assert!(matches!(
            second.status(),
            ProjectStatus::Starting | ProjectStatus::Ready
        ));
        assert_eq!(second.runtime().generation(), first.runtime().generation());
        assert_eq!(fs::read_to_string(&counter).unwrap(), "1");

        let state = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let state = actor.query().await.unwrap();
                if state.status() == ProjectStatus::Ready {
                    break state;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(state.runtime().generation(), first.runtime().generation());
    }

    #[tokio::test]
    async fn project_actor_marks_current_server_exit_failed_but_ignores_stale_exit() {
        let actor = spawn_project_actor(2);
        actor.set_status(ProjectStatus::Ready).await.unwrap();

        actor
            .sender
            .send(ProjectRequest::ServerExited { generation: 99 })
            .await
            .unwrap();
        assert_eq!(actor.query().await.unwrap().status(), ProjectStatus::Ready);

        actor.restart().await.unwrap();
        actor
            .sender
            .send(ProjectRequest::ServerExited { generation: 0 })
            .await
            .unwrap();
        assert_eq!(actor.query().await.unwrap().status(), ProjectStatus::Ready);

        actor
            .sender
            .send(ProjectRequest::ServerExited { generation: 1 })
            .await
            .unwrap();
        let state = actor.query().await.unwrap();
        assert_eq!(state.status(), ProjectStatus::Ready);
        assert_eq!(state.runtime().generation(), 2);
    }

    #[tokio::test]
    async fn project_actor_fails_when_current_server_exits_during_starting() {
        let actor = spawn_project_actor(2);

        actor
            .sender
            .send(ProjectRequest::ServerExited { generation: 0 })
            .await
            .unwrap();

        let state = actor.query().await.unwrap();
        assert_eq!(state.status(), ProjectStatus::Failed);
        assert_eq!(state.last_error(), Some("language server exited"));
    }

    #[tokio::test]
    async fn project_actor_restarts_after_current_server_exit() {
        let actor = spawn_project_actor(2);
        actor.set_status(ProjectStatus::Ready).await.unwrap();

        actor
            .sender
            .send(ProjectRequest::ServerExited { generation: 0 })
            .await
            .unwrap();

        let state = tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                let state = actor.query().await.unwrap();
                if state.status() != ProjectStatus::Restarting {
                    break state;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        assert_eq!(state.status(), ProjectStatus::Ready);
        assert_eq!(state.runtime().generation(), 1);
    }

    #[test]
    fn automatic_restart_policy_is_bounded_and_resettable() {
        let mut policy = AutomaticRestartPolicy::default();

        assert_eq!(
            policy.next().map(|attempt| (attempt.number, attempt.delay)),
            Some((1, Duration::from_millis(100)))
        );
        assert_eq!(
            policy.next().map(|attempt| (attempt.number, attempt.delay)),
            Some((2, Duration::from_millis(500)))
        );
        assert_eq!(
            policy.next().map(|attempt| (attempt.number, attempt.delay)),
            Some((3, Duration::from_secs(2)))
        );
        assert_eq!(policy.next(), None);

        policy.reset();
        assert_eq!(
            policy.next().map(|attempt| (attempt.number, attempt.delay)),
            Some((1, Duration::from_millis(100)))
        );
    }

    #[tokio::test]
    async fn project_actor_retries_failed_restarts_until_the_policy_is_exhausted() {
        let root = TempDir::new().unwrap();
        let mut config = crate::config::LspServerConfig::rust_analyzer();
        config.command = "/definitely/missing/mcpls-language-server".to_string();
        config.heuristics = None;
        let mut translator = Translator::new();
        translator.set_workspace_roots(vec![root.path().to_path_buf()]);
        translator.set_lsp_configs(vec![config], None);
        let actor = spawn_project_actor_with_translator(2, translator);
        actor.set_status(ProjectStatus::Ready).await.unwrap();

        actor
            .sender
            .send(ProjectRequest::ServerExited { generation: 0 })
            .await
            .unwrap();

        let state = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let state = actor.query().await.unwrap();
                if state.status() == ProjectStatus::Failed {
                    break state;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        assert_eq!(state.runtime().generation(), 3);
        assert!(
            state
                .last_error()
                .is_some_and(|error| error.contains("No such file") || error.contains("not found"))
        );
    }

    #[tokio::test]
    async fn project_actor_rejects_queued_semantic_work_after_restart_exhaustion() {
        let root = TempDir::new().unwrap();
        fs::write(
            root.path().join("Cargo.toml"),
            "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        )
        .unwrap();
        fs::create_dir(root.path().join("src")).unwrap();
        fs::write(root.path().join("src/main.rs"), "fn main() {}\n").unwrap();
        let mut config = crate::config::LspServerConfig::rust_analyzer();
        config.command = "/definitely/missing/mcpls-language-server".to_string();
        config.heuristics = None;
        let mut extensions = HashMap::new();
        extensions.insert("rs".to_string(), "rust".to_string());
        let mut translator = Translator::new().with_extensions(extensions);
        translator.set_workspace_roots(vec![root.path().to_path_buf()]);
        translator.set_lsp_configs(vec![config], None);
        let actor = spawn_project_actor_with_translator(2, translator);
        actor.set_status(ProjectStatus::Ready).await.unwrap();

        actor
            .sender
            .send(ProjectRequest::ServerExited { generation: 0 })
            .await
            .unwrap();
        let result = actor
            .document_symbols(root.path().join("src/main.rs").display().to_string())
            .await;

        assert!(matches!(
            result,
            Err(ProjectActorError::Operation(message)) if message == "language server exited"
        ));
    }

    #[tokio::test]
    async fn project_actor_can_add_a_linked_workspace_root() {
        let first = TempDir::new().unwrap();
        let second = TempDir::new().unwrap();
        let first_root = CanonicalRoot::new(first.path()).unwrap();
        let second_root = CanonicalRoot::new(second.path()).unwrap();
        let actor = spawn_project_actor_for_root(2, &first_root);

        let state = actor
            .add_workspace_root(second_root.as_path().to_path_buf())
            .await
            .unwrap();

        assert_eq!(state.workspace_roots().len(), 2);
        assert!(
            state
                .workspace_roots()
                .contains(&first_root.as_path().to_path_buf())
        );
        assert!(
            state
                .workspace_roots()
                .contains(&second_root.as_path().to_path_buf())
        );
        let restarted = actor.restart().await.unwrap();
        assert_eq!(restarted.workspace_roots(), state.workspace_roots());
    }

    #[tokio::test]
    async fn project_actor_owns_bounded_edit_plans() {
        let actor = spawn_project_actor(2);
        let plan = crate::edit_plan::EditPlan::new(
            "project".to_string(),
            Vec::new(),
            Vec::new(),
            true,
            std::time::Duration::from_secs(60),
        );
        let plan_id = plan.id().clone();

        actor.store_edit_plan(plan).await.unwrap();
        let taken = actor
            .take_edit_plan(plan_id.clone(), "project".to_string())
            .await
            .unwrap();
        assert_eq!(taken.project_id(), "project");
        assert!(matches!(
            actor
                .take_edit_plan(plan_id, "project".to_string())
                .await,
            Err(ProjectActorError::Operation(message)) if message.contains("not found")
        ));
    }

    #[tokio::test]
    async fn project_runtime_moves_from_authoritative_open_document_content() {
        let root = TempDir::new().unwrap();
        let source = root.path().join("lib.rs");
        fs::write(&source, "pub mod feature { fn disk() {} }\n").unwrap();
        let dirty = "// dirty\npub mod feature { fn open() {} }\n";
        let mut translator = Translator::new();
        translator.set_workspace_roots(vec![root.path().to_path_buf()]);
        translator
            .document_tracker_mut()
            .open(source.clone(), dirty.to_string())
            .unwrap();
        let mut runtime = ProjectRuntime::new(translator);

        let artifact = runtime
            .move_inline_module_preview(
                "project",
                &source.display().to_string(),
                "feature",
                None,
                PositionEncoding::Utf8,
                root.path(),
            )
            .await
            .unwrap();
        assert_eq!(
            artifact.verification,
            Some(VerificationStatus::StructuralUnverified)
        );
        assert_eq!(artifact.producer, Some(EditProducer::StructuralAstGrep));
        let destination = root.path().join("feature.rs");
        assert!(
            artifact
                .plan
                .files()
                .iter()
                .any(|file| file.path() == &destination && file.was_created())
        );
        assert!(
            artifact
                .plan
                .files()
                .iter()
                .any(|file| file.path() == &source && file.original_content() == dirty)
        );

        let plan_id = artifact.plan.id().clone();
        let applied = runtime
            .apply_edit_plan_with_context(&plan_id, "project", root.path(), None, None)
            .await
            .unwrap();
        assert_eq!(
            applied.verification,
            Some(VerificationStatus::StructuralUnverified)
        );
        assert_eq!(
            runtime
                .translator
                .document_tracker()
                .get(&source)
                .unwrap()
                .content(),
            "// dirty\n#[path = \"feature.rs\"] pub mod feature;\n"
        );
        assert_eq!(fs::read_to_string(destination).unwrap(), " fn open() {} ");
    }

    #[test]
    fn path_rename_composition_uses_authoritative_open_document_content() {
        let root = TempDir::new().unwrap();
        let source = root.path().join("old.rs");
        let destination = root.path().join("renamed.rs");
        let reference = root.path().join("reference.rs");
        fs::write(&source, "pub fn old() {}\n").unwrap();
        fs::write(&reference, "old_name();\n").unwrap();
        let dirty = "old_name(); // dirty\n";

        let reference_uri = path_to_uri(&reference).unwrap().to_string();
        let edit = serde_json::from_value(serde_json::json!({
            "changes": {
                (reference_uri): [{
                    "range": {
                        "start": {"line": 0, "character": 0},
                        "end": {"line": 0, "character": 8}
                    },
                    "newText": "new_name"
                }]
            }
        }))
        .unwrap();
        let (edit, providers, semantic_edit_count) = compose_path_rename_edit(
            WillRenameFilesResult {
                providers: vec!["rust".to_string()],
                edits: vec![edit],
            },
            &source,
            &destination,
        )
        .unwrap();

        let mut translator = Translator::new();
        translator.set_workspace_roots(vec![root.path().to_path_buf()]);
        translator
            .document_tracker_mut()
            .open(reference.clone(), dirty.to_string())
            .unwrap();
        let mut runtime = ProjectRuntime::new(translator);
        let artifact = runtime
            .preview_edit("project", edit, PositionEncoding::Utf8, root.path())
            .unwrap();

        assert_eq!(providers, ["rust"]);
        assert_eq!(semantic_edit_count, 1);
        let snapshot = artifact
            .plan
            .files()
            .iter()
            .find(|snapshot| snapshot.path() == &reference)
            .unwrap();
        assert_eq!(
            snapshot.source(),
            crate::edit_plan::SnapshotSource::OpenDocument
        );
        assert_eq!(snapshot.original_content(), dirty);
        assert_eq!(snapshot.planned_content(), "new_name(); // dirty\n");
        assert_eq!(
            artifact
                .plan
                .operations()
                .iter()
                .filter(|operation| operation.starts_with("rename "))
                .count(),
            1
        );
    }

    #[test]
    fn identifies_rust_analyzer_assists_by_stable_data_id() {
        let matching = lsp_types::CodeAction {
            title: "localized or changed title".to_string(),
            data: Some(serde_json::json!({
                "id": "move_module_to_file:RefactorExtract:2:"
            })),
            ..lsp_types::CodeAction::default()
        };
        let similarly_named = lsp_types::CodeAction {
            title: "Extract module to file".to_string(),
            data: Some(serde_json::json!({
                "id": "move_module_to_file_elsewhere:RefactorExtract:2:"
            })),
            ..lsp_types::CodeAction::default()
        };

        assert!(code_action_has_assist_id(&matching, "move_module_to_file"));
        assert!(!code_action_has_assist_id(
            &similarly_named,
            "move_module_to_file"
        ));
        assert!(
            take_code_action_by_assist_id(
                vec![lsp_types::CodeActionOrCommand::CodeAction(similarly_named)],
                "move_module_to_file",
            )
            .is_none()
        );
        assert_eq!(
            take_code_action_by_assist_id(
                vec![lsp_types::CodeActionOrCommand::CodeAction(matching)],
                "move_module_to_file",
            )
            .map(|action| action.title),
            Some("localized or changed title".to_string())
        );
    }

    #[tokio::test]
    async fn project_runtime_applies_configured_audit_and_backup_policies() {
        let root = TempDir::new().unwrap();
        let file = root.path().join("configured.rs");
        fs::write(&file, "before\n").unwrap();
        let safety = EditSafetyConfig {
            audit_log: Some(crate::config::AuditLogConfig {
                path: PathBuf::from(".mcpls/audit.jsonl"),
                max_bytes: 4_096,
                failure_mode: crate::edit_plan::AuditFailureMode::FailClosed,
            }),
            backup: Some(crate::config::BackupConfig {
                root: PathBuf::from(".mcpls/backups"),
                max_archives: 2,
                max_bytes: 16_384,
                failure_mode: crate::edit_backup::BackupFailureMode::FailClosed,
            }),
        };
        let mut runtime = ProjectRuntime::with_edit_safety(Translator::new(), Some(safety));
        let boundary = WorkspaceBoundary::new(root.path()).unwrap();
        let configured_backup = runtime.configure_edit_safety(&boundary).unwrap().unwrap();
        assert_eq!(
            configured_backup.root(),
            root.path().join(".mcpls/backups").as_path()
        );
        let plan = EditPlan::new(
            "project".to_string(),
            vec![crate::edit_plan::FileSnapshot::from_contents(
                file.clone(),
                crate::edit_plan::SnapshotSource::Disk,
                None,
                "before\n",
                "after\n",
            )],
            Vec::new(),
            true,
            Duration::from_secs(60),
        );
        let plan_id = plan.id().clone();
        runtime.store_edit_plan(plan).unwrap();

        runtime
            .apply_edit_plan_with_context(
                &plan_id,
                "project",
                root.path(),
                Some("session-1".to_string()),
                Some("principal-1".to_string()),
            )
            .await
            .unwrap();

        assert_eq!(fs::read_to_string(&file).unwrap(), "after\n");
        let audit = runtime.edit_plans.audit_records().next().unwrap();
        assert_eq!(audit.session_id(), Some("session-1"));
        assert_eq!(audit.principal(), Some("principal-1"));
        let audit_path = root.path().join(".mcpls/audit.jsonl");
        assert!(
            fs::read_to_string(audit_path)
                .unwrap()
                .contains("Committed")
        );
        assert!(
            root.path()
                .join(".mcpls/backups")
                .join(plan_id.as_str())
                .join("manifest.json")
                .is_file()
        );
    }

    #[tokio::test]
    async fn registry_keeps_one_logical_project_for_linked_git_worktrees() {
        let repository = TempDir::new().unwrap();
        let git_dir = repository.path().join(".git");
        let worktree_git_dir = git_dir.join("worktrees").join("linked");
        fs::create_dir_all(&worktree_git_dir).unwrap();
        fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        fs::write(git_dir.join("config"), "[core]\n").unwrap();
        fs::create_dir(git_dir.join("objects")).unwrap();
        fs::write(worktree_git_dir.join("commondir"), "../..\n").unwrap();

        let worktree = TempDir::new().unwrap();
        fs::write(
            worktree.path().join(".git"),
            format!("gitdir: {}\n", worktree_git_dir.display()),
        )
        .unwrap();
        write_compatible_roots_with_changed_manifests(repository.path(), worktree.path());
        let main_repository = GitRepositoryIdentity::discover(repository.path())
            .unwrap()
            .unwrap();
        let linked_repository = GitRepositoryIdentity::discover(worktree.path())
            .unwrap()
            .unwrap();
        let registry = ProjectRegistry::new(2);
        let main_id = ProjectId::new("main").unwrap();

        let main_actor = registry
            .add(
                ProjectIdentity::new(
                    main_id.clone(),
                    CanonicalRoot::new(repository.path()).unwrap(),
                )
                .with_repository_identity(main_repository),
            )
            .await
            .unwrap();
        let linked_actor = registry
            .add(
                ProjectIdentity::new(
                    main_id.clone(),
                    CanonicalRoot::new(worktree.path()).unwrap(),
                )
                .with_repository_identity(linked_repository),
            )
            .await
            .unwrap();

        let main_state = main_actor.query().await.unwrap();
        let linked_state = linked_actor.query().await.unwrap();
        assert_eq!(main_state.workspace_roots(), linked_state.workspace_roots());
        assert_eq!(main_state.workspace_roots().len(), 2);
        assert_eq!(registry.list().await.len(), 1);

        registry.remove(main_id).await.unwrap();
        assert_eq!(*linked_actor.status().borrow(), ProjectStatus::Stopped);
    }

    #[tokio::test]
    async fn registry_keeps_linked_worktrees_with_different_toolchains_isolated() {
        let repository = TempDir::new().unwrap();
        let git_dir = repository.path().join(".git");
        let worktree_git_dir = git_dir.join("worktrees").join("linked");
        fs::create_dir_all(&worktree_git_dir).unwrap();
        fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        fs::write(git_dir.join("config"), "[core]\n").unwrap();
        fs::create_dir(git_dir.join("objects")).unwrap();
        fs::write(worktree_git_dir.join("commondir"), "../..\n").unwrap();

        let worktree = TempDir::new().unwrap();
        fs::write(
            worktree.path().join(".git"),
            format!("gitdir: {}\n", worktree_git_dir.display()),
        )
        .unwrap();
        fs::write(
            repository.path().join("rust-toolchain.toml"),
            "[toolchain]\nchannel = \"stable\"\n",
        )
        .unwrap();
        fs::write(
            worktree.path().join("rust-toolchain.toml"),
            "[toolchain]\nchannel = \"nightly\"\n",
        )
        .unwrap();
        for root in [repository.path(), worktree.path()] {
            fs::write(root.join("Cargo.toml"), "[package]\nname = \"fixture\"\n").unwrap();
        }
        let main_repository = GitRepositoryIdentity::discover(repository.path())
            .unwrap()
            .unwrap();
        let linked_repository = GitRepositoryIdentity::discover(worktree.path())
            .unwrap()
            .unwrap();
        let registry = ProjectRegistry::new(2);

        let main_actor = registry
            .add(
                ProjectIdentity::new(
                    ProjectId::new("main").unwrap(),
                    CanonicalRoot::new(repository.path()).unwrap(),
                )
                .with_repository_identity(main_repository),
            )
            .await
            .unwrap();
        let linked_actor = registry
            .add(
                ProjectIdentity::new(
                    ProjectId::new("main").unwrap(),
                    CanonicalRoot::new(worktree.path()).unwrap(),
                )
                .with_repository_identity(linked_repository),
            )
            .await;
        let linked_actor = linked_actor.unwrap();

        assert_eq!(main_actor.query().await.unwrap().workspace_roots().len(), 1);
        assert_eq!(
            linked_actor.query().await.unwrap().workspace_roots().len(),
            1
        );
        assert!(!main_actor.sender.same_channel(&linked_actor.sender));
        assert_eq!(registry.list().await.len(), 1);
        assert_eq!(
            registry
                .actor_group_count(&ProjectId::new("main").unwrap())
                .await
                .unwrap(),
            2
        );
        registry
            .remove(ProjectId::new("main").unwrap())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn registry_resolves_semantic_paths_to_the_longest_project_actor() {
        let root = TempDir::new().unwrap();
        let nested = root.path().join("nested");
        fs::create_dir(&nested).unwrap();
        let file = nested.join("src.rs");
        fs::write(&file, "fn main() {}\n").unwrap();
        let registry = ProjectRegistry::new(2);
        let outer = ProjectId::new("outer").unwrap();
        let inner = ProjectId::new("inner").unwrap();
        registry
            .add(ProjectIdentity::new(
                outer.clone(),
                CanonicalRoot::new(root.path()).unwrap(),
            ))
            .await
            .unwrap();
        let inner_actor = registry
            .add(ProjectIdentity::new(
                inner,
                CanonicalRoot::new(&nested).unwrap(),
            ))
            .await
            .unwrap();

        let resolved = registry.actor_for_path(&file).await.unwrap();

        assert_eq!(
            resolved.query().await.unwrap().workspace_roots(),
            inner_actor.query().await.unwrap().workspace_roots()
        );
    }

    #[tokio::test]
    async fn project_actor_activation_owns_lsp_failure_state() {
        let root = TempDir::new().unwrap();
        fs::write(
            root.path().join("Cargo.toml"),
            "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        )
        .unwrap();
        let mut config = crate::config::LspServerConfig::rust_analyzer();
        config.command = "/definitely/missing/rust-analyzer".to_string();
        let mut translator = Translator::new();
        translator.set_lsp_configs(vec![config], Some(1));
        let handle = spawn_project_actor_with_translator(2, translator);

        let result = handle.activate(root.path().to_path_buf()).await;

        assert!(matches!(result, Err(ProjectActorError::Operation(_))));
        let state = handle.query().await.unwrap();
        assert_eq!(state.status(), ProjectStatus::Failed);
        assert_eq!(
            state.runtime().configured_language_ids(),
            &["rust".to_string()]
        );
        assert!(state.last_error().is_some());
    }

    #[tokio::test]
    async fn project_registry_adds_lists_and_removes_without_duplicate_actors() {
        let root = TempDir::new().unwrap();
        let identity = ProjectIdentity::new(
            ProjectId::new("demo").unwrap(),
            CanonicalRoot::new(root.path()).unwrap(),
        );
        let registry = ProjectRegistry::new(4);

        registry.add(identity.clone()).await.unwrap();
        let duplicate = registry.add(identity).await.unwrap();
        assert_eq!(registry.list().await.len(), 1);
        let state = duplicate.query().await.unwrap();
        assert_eq!(state.status(), ProjectStatus::Starting);
        assert_eq!(state.workspace_roots().len(), 1);

        registry
            .remove(ProjectId::new("demo").unwrap())
            .await
            .unwrap();
        assert!(
            duplicate
                .event_snapshot(None)
                .events()
                .iter()
                .any(|record| {
                    record.event()
                        == &ProjectEvent::ProjectRemoved {
                            project_id: ProjectId::new("demo").unwrap(),
                            root: root.path().canonicalize().unwrap(),
                        }
                })
        );
        assert!(registry.list().await.is_empty());
    }

    #[tokio::test]
    async fn project_registry_retains_bounded_history_after_removal() {
        let root = TempDir::new().unwrap();
        let project_id = ProjectId::new("removed-history").unwrap();
        let registry = ProjectRegistry::new(2);
        let actor = registry
            .add(ProjectIdentity::new(
                project_id.clone(),
                CanonicalRoot::new(root.path()).unwrap(),
            ))
            .await
            .unwrap();

        actor
            .sender
            .send(ProjectRequest::Notification {
                generation: 0,
                server_id: ServerId::from("rust"),
                notification: LspNotification::parse(
                    "window/logMessage",
                    Some(serde_json::json!({"type": 1, "message": "retained log"})),
                ),
            })
            .await
            .unwrap();
        actor
            .sender
            .send(ProjectRequest::Notification {
                generation: 0,
                server_id: ServerId::from("rust"),
                notification: LspNotification::parse(
                    "window/showMessage",
                    Some(serde_json::json!({"type": 2, "message": "retained message"})),
                ),
            })
            .await
            .unwrap();

        assert_eq!(actor.server_logs(10, None).await.unwrap().logs.len(), 1);

        registry.remove(project_id.clone()).await.unwrap();

        let logs = registry.server_logs(&project_id, 10, None).await.unwrap();
        assert_eq!(logs.logs[0].message, "retained log");
        let messages = registry.server_messages(&project_id, 10).await.unwrap();
        assert_eq!(messages.messages[0].message, "retained message");
    }

    #[tokio::test]
    async fn project_registry_evicts_old_removed_history() {
        let registry = ProjectRegistry::new(1);
        let roots = (0..=RETAINED_PROJECT_HISTORY_CAPACITY)
            .map(|_| TempDir::new().unwrap())
            .collect::<Vec<_>>();

        for (index, root) in roots.iter().enumerate() {
            let id = ProjectId::new(format!("removed-{index}")).unwrap();
            registry
                .add(ProjectIdentity::new(
                    id.clone(),
                    CanonicalRoot::new(root.path()).unwrap(),
                ))
                .await
                .unwrap();
            registry.remove(id).await.unwrap();
        }

        let oldest = ProjectId::new("removed-0").unwrap();
        assert!(matches!(
            registry.server_logs(&oldest, 10, None).await,
            Err(ProjectRegistryError::ProjectNotFound(_))
        ));
        let newest =
            ProjectId::new(format!("removed-{RETAINED_PROJECT_HISTORY_CAPACITY}")).unwrap();
        assert!(registry.server_logs(&newest, 10, None).await.is_ok());
    }

    #[tokio::test]
    async fn project_registry_persists_add_and_remove_mutations() {
        let root = TempDir::new().unwrap();
        let state_path = root.path().join("state/projects.json");
        let store = ProjectRegistrationStore::new(&state_path);
        let registry = ProjectRegistry::new(2).with_persistence(store.clone());
        let identity = ProjectIdentity::new(
            ProjectId::new("persisted").unwrap(),
            CanonicalRoot::new(root.path()).unwrap(),
        );

        registry.add(identity).await.unwrap();
        assert_eq!(store.load().unwrap().projects.len(), 1);

        registry
            .remove(ProjectId::new("persisted").unwrap())
            .await
            .unwrap();
        assert!(store.load().unwrap().projects.is_empty());
    }

    #[tokio::test]
    async fn project_registry_keeps_failed_removal_registered_and_persisted() {
        let root = TempDir::new().unwrap();
        let state_path = root.path().join("state/projects.json");
        let store = ProjectRegistrationStore::new(&state_path);
        let registry = ProjectRegistry::new(2).with_persistence(store.clone());
        let project_id = ProjectId::new("failed-removal").unwrap();
        let identity =
            ProjectIdentity::new(project_id.clone(), CanonicalRoot::new(root.path()).unwrap());
        let (sender, receiver) = mpsc::channel(1);
        drop(receiver);
        let (_, status) = watch::channel(ProjectStatus::Starting);
        let (events, _) = broadcast::channel(1);
        registry.projects.write().await.insert(
            project_id.clone(),
            ProjectEntry {
                identity: identity.clone(),
                actors: vec![ProjectActorEntry {
                    actor: ProjectHandle {
                        sender: ProjectRequestSender::new(sender),
                        status,
                        events,
                        event_history: std::sync::Arc::new(std::sync::Mutex::new(
                            ProjectEventHistory::new(1),
                        )),
                    },
                    mutation: std::sync::Arc::new(Mutex::new(())),
                    compatibility: ProjectCompatibility::Resolved(None),
                    translator_template: None,
                    roots: vec![identity.root().clone()],
                }],
                config: None,
            },
        );
        store
            .save(&[PersistedProject {
                project_id: project_id.to_string(),
                root: identity.root().as_path().to_path_buf(),
                additional_roots: Vec::new(),
                config: None,
            }])
            .unwrap();

        let result = registry.remove(project_id.clone()).await;

        assert!(matches!(
            result,
            Err(ProjectRegistryError::Actor(ProjectActorError::Closed))
        ));
        assert_eq!(registry.list().await, vec![identity]);
        assert_eq!(store.load().unwrap().projects.len(), 1);
    }

    #[tokio::test]
    async fn project_registry_reopens_work_after_failed_actor_shutdown() {
        let root = TempDir::new().unwrap();
        let registry = ProjectRegistry::new(2);
        let project_id = ProjectId::new("retry-removal").unwrap();
        let identity =
            ProjectIdentity::new(project_id.clone(), CanonicalRoot::new(root.path()).unwrap());
        let (sender, mut receiver) = mpsc::channel(2);
        tokio::spawn(async move {
            while let Some(request) = receiver.recv().await {
                match request {
                    ProjectRequest::PublishEvent { reply, .. }
                    | ProjectRequest::SetStatus { reply, .. } => {
                        let _ = reply.send(());
                    }
                    _ => {}
                }
            }
        });
        let (_, status) = watch::channel(ProjectStatus::Starting);
        let (events, _) = broadcast::channel(1);
        let actor = ProjectHandle {
            sender: ProjectRequestSender::new(sender),
            status,
            events,
            event_history: std::sync::Arc::new(std::sync::Mutex::new(ProjectEventHistory::new(1))),
        };
        registry.projects.write().await.insert(
            project_id.clone(),
            ProjectEntry {
                identity,
                actors: vec![ProjectActorEntry {
                    actor: actor.clone(),
                    mutation: std::sync::Arc::new(Mutex::new(())),
                    compatibility: ProjectCompatibility::Resolved(None),
                    translator_template: None,
                    roots: vec![CanonicalRoot::new(root.path()).unwrap()],
                }],
                config: None,
            },
        );

        assert!(matches!(
            registry.remove(project_id).await,
            Err(ProjectRegistryError::Actor(ProjectActorError::Cancelled))
        ));
        assert!(actor.set_status(ProjectStatus::Ready).await.is_ok());
    }

    #[tokio::test]
    async fn project_registry_reports_persistence_and_shutdown_state() {
        let transient = ProjectRegistry::new(2);
        assert!(!transient.persistence_configured());
        assert!(!transient.is_shutting_down());

        let state_path = tempfile::tempdir().unwrap().path().join("projects.json");
        let persistent =
            ProjectRegistry::new(2).with_persistence(ProjectRegistrationStore::new(state_path));
        assert!(persistent.persistence_configured());
        persistent.shutdown_all().await;
        assert!(persistent.is_shutting_down());
    }

    #[tokio::test]
    async fn project_registry_restores_existing_roots_and_prunes_missing_roots() {
        let root = TempDir::new().unwrap();
        let state_path = root.path().join("state/projects.json");
        let store = ProjectRegistrationStore::new(&state_path);
        store
            .save(&[
                PersistedProject {
                    project_id: "existing".to_string(),
                    root: root.path().to_path_buf(),
                    additional_roots: Vec::new(),
                    config: None,
                },
                PersistedProject {
                    project_id: "missing".to_string(),
                    root: root.path().join("gone"),
                    additional_roots: Vec::new(),
                    config: None,
                },
            ])
            .unwrap();
        let registry = ProjectRegistry::new(2).with_persistence(store.clone());

        assert_eq!(registry.restore_from_persistence().await.unwrap(), 1);
        assert_eq!(registry.list().await.len(), 1);
        assert_eq!(registry.list().await[0].id().as_str(), "existing");
        assert_eq!(store.load().unwrap().projects.len(), 1);
    }

    #[tokio::test]
    async fn project_registry_defers_compatibility_for_a_restored_single_root() {
        let root = TempDir::new().unwrap();
        let store = ProjectRegistrationStore::new(root.path().join("state/projects.json"));
        store
            .save(&[PersistedProject {
                project_id: "dormant".to_string(),
                root: root.path().to_path_buf(),
                additional_roots: Vec::new(),
                config: None,
            }])
            .unwrap();
        let registry = ProjectRegistry::new(2).with_persistence(store);

        registry.restore_from_persistence().await.unwrap();

        let projects = registry.projects.read().await;
        assert_eq!(
            projects[&ProjectId::new("dormant").unwrap()]
                .primary()
                .compatibility,
            ProjectCompatibility::Deferred
        );
        drop(projects);
    }

    #[tokio::test]
    async fn adding_a_linked_root_resolves_deferred_compatibility() {
        let repository = TempDir::new().unwrap();
        let git_dir = repository.path().join(".git");
        let worktree_git_dir = git_dir.join("worktrees/linked");
        fs::create_dir_all(&worktree_git_dir).unwrap();
        fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        fs::write(git_dir.join("config"), "[core]\n").unwrap();
        fs::create_dir(git_dir.join("objects")).unwrap();
        fs::write(worktree_git_dir.join("commondir"), "../..\n").unwrap();
        let worktree = TempDir::new().unwrap();
        fs::write(
            worktree.path().join(".git"),
            format!("gitdir: {}\n", worktree_git_dir.display()),
        )
        .unwrap();
        write_compatible_roots_with_changed_manifests(repository.path(), worktree.path());
        let project_id = ProjectId::new("repository").unwrap();
        let store = ProjectRegistrationStore::new(repository.path().join("state/projects.json"));
        store
            .save(&[PersistedProject {
                project_id: project_id.to_string(),
                root: repository.path().to_path_buf(),
                additional_roots: Vec::new(),
                config: None,
            }])
            .unwrap();
        let registry = ProjectRegistry::new(2).with_persistence(store);
        registry.restore_from_persistence().await.unwrap();
        let linked_repository = GitRepositoryIdentity::discover(worktree.path())
            .unwrap()
            .unwrap();

        registry
            .add(
                ProjectIdentity::new(
                    project_id.clone(),
                    CanonicalRoot::new(worktree.path()).unwrap(),
                )
                .with_repository_identity(linked_repository),
            )
            .await
            .unwrap();

        assert_eq!(registry.actor_group_count(&project_id).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn project_registry_persists_project_configuration() {
        let root = TempDir::new().unwrap();
        fs::write(
            root.path().join("Cargo.toml"),
            "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        )
        .unwrap();
        let store = ProjectRegistrationStore::new(root.path().join("state/projects.json"));
        let registry = ProjectRegistry::new(2).with_persistence(store.clone());
        let mut server = crate::config::LspServerConfig::rust_analyzer();
        server.command = "/definitely/missing/rust-analyzer".to_string();
        let config = ProjectConfig {
            lsp_servers: Some(vec![server]),
            heuristics_max_depth: Some(3),
            redaction_patterns: None,
            persist_environment: false,
            edit_safety: None,
        };

        registry
            .add_with_config(
                ProjectIdentity::new(
                    ProjectId::new("configured").unwrap(),
                    CanonicalRoot::new(root.path()).unwrap(),
                ),
                Some(config.clone()),
            )
            .await
            .unwrap();

        assert_eq!(store.load().unwrap().projects[0].config, Some(config));

        let restored = ProjectRegistry::new(2).with_persistence(store);
        assert_eq!(restored.restore_from_persistence().await.unwrap(), 1);
        let state = restored
            .actor_for_project(&ProjectId::new("configured").unwrap())
            .await
            .unwrap()
            .query()
            .await
            .unwrap();
        assert_eq!(
            state.runtime().configured_language_ids(),
            vec!["rust".to_string()]
        );
    }

    #[tokio::test]
    async fn project_registry_activation_uses_configuration_snapshot() {
        let root = TempDir::new().unwrap();
        fs::write(
            root.path().join("Cargo.toml"),
            "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        )
        .unwrap();
        let mut translator = Translator::new();
        let mut config = crate::config::LspServerConfig::rust_analyzer();
        config.command = "/definitely/missing/rust-analyzer".to_string();
        translator.set_lsp_configs(vec![config], Some(1));
        let registry =
            ProjectRegistry::with_translator_template(2, translator.configuration_template());
        let id = ProjectId::new("fixture").unwrap();
        registry
            .add(ProjectIdentity::new(
                id.clone(),
                CanonicalRoot::new(root.path()).unwrap(),
            ))
            .await
            .unwrap();

        let result = registry.activate(&id).await;

        assert!(matches!(
            result,
            Err(ProjectRegistryError::Actor(ProjectActorError::Operation(_)))
        ));
        let state = registry.status(&id).await.unwrap();
        assert_eq!(state.status(), ProjectStatus::Failed);
        assert_eq!(
            state.runtime().configured_language_ids(),
            &["rust".to_string()]
        );
    }

    #[tokio::test]
    async fn nested_cargo_project_is_governed_by_rust_residency_budget() {
        let root = TempDir::new().unwrap();
        let nested = root.path().join("crates/fixture");
        fs::create_dir_all(&nested).unwrap();
        fs::write(
            nested.join("Cargo.toml"),
            "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        )
        .unwrap();
        let mut translator = Translator::new();
        translator.set_lsp_configs(
            vec![crate::config::LspServerConfig::rust_analyzer()],
            Some(10),
        );
        let registry =
            ProjectRegistry::with_translator_template(2, translator.configuration_template());
        let id = ProjectId::new("nested").unwrap();
        registry
            .add(ProjectIdentity::new(
                id.clone(),
                CanonicalRoot::new(root.path()).unwrap(),
            ))
            .await
            .unwrap();

        let projects = registry.projects.read().await;
        let governed = projects.get(&id).unwrap().actors[0]
            .actor
            .sender
            .residency
            .is_some();
        drop(projects);
        assert!(governed);
    }

    #[tokio::test]
    async fn project_registry_keeps_independent_projects_isolated() {
        let first_root = TempDir::new().unwrap();
        let second_root = TempDir::new().unwrap();
        let first = ProjectIdentity::new(
            ProjectId::new("first").unwrap(),
            CanonicalRoot::new(first_root.path()).unwrap(),
        );
        let second = ProjectIdentity::new(
            ProjectId::new("second").unwrap(),
            CanonicalRoot::new(second_root.path()).unwrap(),
        );
        let registry = ProjectRegistry::new(2);
        registry.add(first).await.unwrap();
        registry.add(second).await.unwrap();

        registry
            .restart(&ProjectId::new("first").unwrap())
            .await
            .unwrap();
        assert_eq!(
            registry
                .status(&ProjectId::new("second").unwrap())
                .await
                .unwrap()
                .status(),
            ProjectStatus::Starting
        );
    }

    #[tokio::test]
    async fn project_registry_isolates_failed_lsp_recovery_between_projects() {
        let first_root = TempDir::new().unwrap();
        let second_root = TempDir::new().unwrap();
        for root in [first_root.path(), second_root.path()] {
            fs::write(
                root.join("Cargo.toml"),
                "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
            )
            .unwrap();
        }

        let mut broken_server = crate::config::LspServerConfig::rust_analyzer();
        broken_server.command = "/definitely/missing/mcpls-language-server".to_string();
        broken_server.heuristics = None;
        let registry = ProjectRegistry::new(4);
        let first_id = ProjectId::new("first").unwrap();
        let second_id = ProjectId::new("second").unwrap();
        registry
            .add_with_config(
                ProjectIdentity::new(
                    first_id.clone(),
                    CanonicalRoot::new(first_root.path()).unwrap(),
                ),
                Some(ProjectConfig {
                    lsp_servers: Some(vec![broken_server]),
                    heuristics_max_depth: Some(3),
                    redaction_patterns: None,
                    persist_environment: false,
                    edit_safety: None,
                }),
            )
            .await
            .unwrap();
        registry
            .add(ProjectIdentity::new(
                second_id.clone(),
                CanonicalRoot::new(second_root.path()).unwrap(),
            ))
            .await
            .unwrap();

        let first = registry.actor_for_project(&first_id).await.unwrap();
        let second = registry.actor_for_project(&second_id).await.unwrap();
        first.set_status(ProjectStatus::Ready).await.unwrap();
        second.set_status(ProjectStatus::Ready).await.unwrap();
        let mut second_events = second.subscribe_events();
        first
            .sender
            .send(ProjectRequest::ServerExited { generation: 0 })
            .await
            .unwrap();

        let first_state = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let state = first.query().await.unwrap();
                if state.status() == ProjectStatus::Failed {
                    break state;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        assert_eq!(first_state.status(), ProjectStatus::Failed);
        assert_eq!(
            registry.status(&second_id).await.unwrap().status(),
            ProjectStatus::Ready
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(50), second_events.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn project_status_reports_failure_in_any_actor_group() {
        let primary_root = TempDir::new().unwrap();
        let secondary_root = TempDir::new().unwrap();
        let project_id = ProjectId::new("logical").unwrap();
        let primary = spawn_project_actor(2);
        let secondary = spawn_project_actor(2);
        primary.set_status(ProjectStatus::Ready).await.unwrap();
        secondary.fail("secondary toolchain failed").await.unwrap();

        let primary_root = CanonicalRoot::new(primary_root.path()).unwrap();
        let secondary_root = CanonicalRoot::new(secondary_root.path()).unwrap();
        let mut entry = ProjectEntry::new(
            ProjectIdentity::new(project_id.clone(), primary_root.clone()),
            primary,
            std::sync::Arc::new(Mutex::new(())),
            None,
            None,
            None,
        );
        entry.identity.add_root(secondary_root.clone());
        entry.actors.push(ProjectActorEntry::new(
            secondary,
            std::sync::Arc::new(Mutex::new(())),
            None,
            None,
            secondary_root,
        ));

        let registry = ProjectRegistry::new(2);
        registry
            .projects
            .write()
            .await
            .insert(project_id.clone(), entry);

        let state = registry.status(&project_id).await.unwrap();

        assert_eq!(state.status(), ProjectStatus::Failed);
        assert_eq!(state.last_error(), Some("secondary toolchain failed"));
        let counts = registry.status_counts().await;
        assert_eq!(counts.failed, 1);
        assert_eq!(counts.ready, 0);
    }

    #[test]
    fn project_status_aggregate_preserves_ready_for_one_actor() {
        let state = ProjectState::aggregate([ProjectState::new(
            ProjectStatus::Ready,
            ProjectRuntimeSummary::default(),
        )]);

        assert_eq!(state.status(), ProjectStatus::Ready);
    }

    #[test]
    fn partial_activation_is_degraded() {
        assert_eq!(
            activation_status(ActivationHealth::Degraded, false),
            ProjectStatus::Degraded
        );
    }

    #[tokio::test]
    async fn project_registry_serializes_restart_and_remove() {
        let root = TempDir::new().unwrap();
        let registry = ProjectRegistry::new(2);
        let id = ProjectId::new("race").unwrap();
        registry
            .add(ProjectIdentity::new(
                id.clone(),
                CanonicalRoot::new(root.path()).unwrap(),
            ))
            .await
            .unwrap();

        let restart_registry = registry.clone();
        let remove_registry = registry.clone();
        let restart_id = id.clone();
        let remove_id = id.clone();
        let (restart, remove) = tokio::join!(
            restart_registry.restart(&restart_id),
            remove_registry.remove(remove_id),
        );

        assert!(restart.is_ok() || matches!(restart, Err(ProjectRegistryError::Actor(_))));
        assert!(remove.is_ok());
        assert!(registry.list().await.is_empty());
    }

    #[tokio::test]
    async fn project_registry_remove_blocks_re_registration_until_removal_finishes() {
        let root = TempDir::new().unwrap();
        let registry = ProjectRegistry::new(2);
        let id = ProjectId::new("race").unwrap();
        let identity = ProjectIdentity::new(id.clone(), CanonicalRoot::new(root.path()).unwrap());
        registry.add(identity.clone()).await.unwrap();

        let mutation = registry.projects.read().await.get(&id).unwrap().actors[0]
            .mutation
            .clone();
        let guard = mutation.lock().await;
        let remove_registry = registry.clone();
        let remove_id = id.clone();
        let remove = tokio::spawn(async move { remove_registry.remove(remove_id).await });

        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        assert!(matches!(
            tokio::time::timeout(Duration::from_millis(10), registry.add(identity.clone()))
                .await
                .unwrap(),
            Err(ProjectRegistryError::ProjectRemoving(project)) if project == id
        ));

        drop(guard);
        assert!(remove.await.unwrap().is_ok());
        registry.add(identity).await.unwrap();
        assert_eq!(registry.list().await.len(), 1);
    }

    #[tokio::test]
    async fn project_registry_removal_rejects_new_actor_requests() {
        let root = TempDir::new().unwrap();
        let registry = ProjectRegistry::new(2);
        let id = ProjectId::new("removing").unwrap();
        let identity = ProjectIdentity::new(id.clone(), CanonicalRoot::new(root.path()).unwrap());
        let actor = registry.add(identity.clone()).await.unwrap();

        let mutation = registry.projects.read().await.get(&id).unwrap().actors[0]
            .mutation
            .clone();
        let guard = mutation.lock().await;
        let remove_registry = registry.clone();
        let remove_id = id.clone();
        let remove = tokio::spawn(async move { remove_registry.remove(remove_id).await });

        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        assert!(matches!(
            registry.add(identity).await,
            Err(ProjectRegistryError::ProjectRemoving(project)) if project == id
        ));
        assert!(matches!(
            tokio::time::timeout(
                Duration::from_millis(10),
                actor.set_status(ProjectStatus::Ready)
            )
            .await,
            Ok(Err(ProjectActorError::Closed))
        ));

        drop(guard);
        assert!(remove.await.unwrap().is_ok());
    }

    #[tokio::test]
    async fn project_registry_shuts_down_all_registered_actors() {
        let first = TempDir::new().unwrap();
        let second = TempDir::new().unwrap();
        let registry = ProjectRegistry::new(2);
        let first_id = ProjectId::new("first").unwrap();
        let second_id = ProjectId::new("second").unwrap();
        let first_actor = registry
            .add(ProjectIdentity::new(
                first_id.clone(),
                CanonicalRoot::new(first.path()).unwrap(),
            ))
            .await
            .unwrap();
        let second_actor = registry
            .add(ProjectIdentity::new(
                second_id.clone(),
                CanonicalRoot::new(second.path()).unwrap(),
            ))
            .await
            .unwrap();

        let report = registry.shutdown_all().await;

        assert!(report.failed.is_empty());
        assert_eq!(report.stopped, vec![first_id, second_id]);
        assert_eq!(*first_actor.status().borrow(), ProjectStatus::Stopped);
        assert_eq!(*second_actor.status().borrow(), ProjectStatus::Stopped);
    }

    #[tokio::test]
    async fn project_registry_shutdown_rejects_new_actor_requests_before_draining() {
        let root = TempDir::new().unwrap();
        let registry = ProjectRegistry::new(2);
        let project_id = ProjectId::new("shutdown-race").unwrap();
        registry
            .add(ProjectIdentity::new(
                project_id.clone(),
                CanonicalRoot::new(root.path()).unwrap(),
            ))
            .await
            .unwrap();
        let actor = registry.actor_for_project(&project_id).await.unwrap();
        let mutation = registry
            .projects
            .read()
            .await
            .get(&project_id)
            .unwrap()
            .actors[0]
            .mutation
            .clone();
        let guard = mutation.lock().await;

        let shutdown_registry = registry.clone();
        let shutdown = tokio::spawn(async move { shutdown_registry.shutdown_all().await });
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }

        assert!(matches!(
            actor.set_status(ProjectStatus::Ready).await,
            Err(ProjectActorError::Closed)
        ));

        drop(guard);
        let report = shutdown.await.unwrap();
        assert!(report.failed.is_empty());
        assert!(report.stopped.contains(&project_id));
    }

    #[tokio::test]
    async fn project_registry_rejects_registration_after_shutdown_begins() {
        let registry = ProjectRegistry::new(2);
        registry.shutdown_all().await;

        let root = TempDir::new().unwrap();
        let result = registry
            .add(ProjectIdentity::new(
                ProjectId::new("late").unwrap(),
                CanonicalRoot::new(root.path()).unwrap(),
            ))
            .await;

        assert!(matches!(result, Err(ProjectRegistryError::ShuttingDown)));
    }

    #[tokio::test]
    async fn project_registry_reports_shutdown_timeout() {
        let root = TempDir::new().unwrap();
        let registry = ProjectRegistry::new(2).with_shutdown_timeout(Duration::ZERO);
        let project_id = ProjectId::new("slow").unwrap();
        let (sender, mut requests) = mpsc::channel(1);
        tokio::spawn(async move {
            if let Some(ProjectRequest::Shutdown { .. }) = requests.recv().await {
                std::future::pending::<()>().await;
            }
        });
        let (_, status) = watch::channel(ProjectStatus::Starting);
        let (events, _) = broadcast::channel(1);
        let actor = ProjectHandle {
            sender: ProjectRequestSender::new(sender),
            status,
            events,
            event_history: std::sync::Arc::new(std::sync::Mutex::new(ProjectEventHistory::new(1))),
        };
        registry.projects.write().await.insert(
            project_id.clone(),
            ProjectEntry {
                identity: ProjectIdentity::new(
                    project_id.clone(),
                    CanonicalRoot::new(root.path()).unwrap(),
                ),
                actors: vec![ProjectActorEntry {
                    actor,
                    mutation: std::sync::Arc::new(Mutex::new(())),
                    compatibility: ProjectCompatibility::Resolved(None),
                    translator_template: None,
                    roots: vec![CanonicalRoot::new(root.path()).unwrap()],
                }],
                config: None,
            },
        );

        let report = registry.shutdown_all().await;

        assert!(report.stopped.is_empty());
        assert_eq!(report.failed.len(), 1);
        assert_eq!(report.failed[0].project_id, project_id);
        assert_eq!(report.failed[0].error, "shutdown timed out after 0ns");
    }

    #[tokio::test]
    async fn project_registry_shutdown_waits_for_project_mutations() {
        let root = TempDir::new().unwrap();
        let registry = ProjectRegistry::new(2);
        let project_id = ProjectId::new("project").unwrap();
        registry
            .add(ProjectIdentity::new(
                project_id.clone(),
                CanonicalRoot::new(root.path()).unwrap(),
            ))
            .await
            .unwrap();
        let mutation = registry
            .projects
            .read()
            .await
            .get(&project_id)
            .unwrap()
            .actors[0]
            .mutation
            .clone();
        let guard = mutation.lock().await;
        let shutdown_registry = registry.clone();
        let mut shutdown = tokio::spawn(async move { shutdown_registry.shutdown_all().await });

        assert!(
            tokio::time::timeout(Duration::from_millis(10), &mut shutdown)
                .await
                .is_err()
        );
        drop(guard);

        let report = shutdown.await.unwrap();
        assert_eq!(report.stopped, vec![project_id]);
        assert!(report.failed.is_empty());
    }

    #[tokio::test]
    async fn project_registry_rejects_conflicting_duplicate_ids() {
        let first_root = TempDir::new().unwrap();
        let second_root = TempDir::new().unwrap();
        let registry = ProjectRegistry::new(2);
        registry
            .add(ProjectIdentity::new(
                ProjectId::new("same").unwrap(),
                CanonicalRoot::new(first_root.path()).unwrap(),
            ))
            .await
            .unwrap();

        let result = registry
            .add(ProjectIdentity::new(
                ProjectId::new("same").unwrap(),
                CanonicalRoot::new(second_root.path()).unwrap(),
            ))
            .await;

        assert!(matches!(
            result,
            Err(ProjectRegistryError::ConflictingProject { id, .. }) if id.as_str() == "same"
        ));
    }

    #[tokio::test]
    async fn project_registry_adds_compatible_worktree_to_one_logical_project() {
        let repository = TempDir::new().unwrap();
        let git_dir = repository.path().join(".git");
        let worktree_git_dir = git_dir.join("worktrees").join("linked");
        fs::create_dir_all(&worktree_git_dir).unwrap();
        fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        fs::write(git_dir.join("config"), "[core]\n").unwrap();
        fs::create_dir(git_dir.join("objects")).unwrap();
        fs::write(worktree_git_dir.join("commondir"), "../..\n").unwrap();

        let worktree = TempDir::new().unwrap();
        fs::write(
            worktree.path().join(".git"),
            format!("gitdir: {}\n", worktree_git_dir.display()),
        )
        .unwrap();
        for root in [repository.path(), worktree.path()] {
            fs::write(
                root.join("rust-toolchain.toml"),
                "[toolchain]\nchannel = \"stable\"\n",
            )
            .unwrap();
            fs::write(root.join("Cargo.toml"), "[package]\nname = \"fixture\"\n").unwrap();
        }
        let repository_identity = GitRepositoryIdentity::discover(repository.path())
            .unwrap()
            .unwrap();
        let linked_identity = GitRepositoryIdentity::discover(worktree.path())
            .unwrap()
            .unwrap();
        let project_id = ProjectId::new("repository").unwrap();
        let registry = ProjectRegistry::new(2);

        registry
            .add(
                ProjectIdentity::new(
                    project_id.clone(),
                    CanonicalRoot::new(repository.path()).unwrap(),
                )
                .with_repository_identity(repository_identity),
            )
            .await
            .unwrap();
        let wrong_id = ProjectId::new("worktree").unwrap();
        let wrong_id_result = registry
            .add(
                ProjectIdentity::new(wrong_id, CanonicalRoot::new(worktree.path()).unwrap())
                    .with_repository_identity(linked_identity.clone()),
            )
            .await;
        assert!(matches!(
            wrong_id_result,
            Err(ProjectRegistryError::LinkedWorktreeProject { existing_id, .. })
                if existing_id == project_id
        ));
        let actor = registry
            .add(
                ProjectIdentity::new(
                    project_id.clone(),
                    CanonicalRoot::new(worktree.path()).unwrap(),
                )
                .with_repository_identity(linked_identity),
            )
            .await
            .unwrap();

        assert_eq!(registry.list().await.len(), 1);
        assert_eq!(registry.actor_group_count(&project_id).await.unwrap(), 1);
        assert_eq!(actor.query().await.unwrap().workspace_roots().len(), 2);
        let file = worktree.path().join("src.rs");
        fs::write(&file, "fn main() {}\n").unwrap();
        let (resolved_id, resolved_actor) = registry.project_for_path(&file).await.unwrap();
        assert_eq!(resolved_id, project_id);
        assert!(resolved_actor.sender.same_channel(&actor.sender));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn compatible_linked_worktrees_share_one_lsp_process() {
        use std::collections::HashMap;
        use std::os::unix::fs::PermissionsExt;

        let repository = TempDir::new().unwrap();
        let git_dir = repository.path().join(".git");
        let worktree_git_dir = git_dir.join("worktrees").join("linked");
        fs::create_dir_all(&worktree_git_dir).unwrap();
        fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        fs::write(git_dir.join("config"), "[core]\n").unwrap();
        fs::create_dir(git_dir.join("objects")).unwrap();
        fs::write(worktree_git_dir.join("commondir"), "../..\n").unwrap();

        let worktree = TempDir::new().unwrap();
        fs::write(
            worktree.path().join(".git"),
            format!("gitdir: {}\n", worktree_git_dir.display()),
        )
        .unwrap();
        write_compatible_roots_with_changed_manifests(repository.path(), worktree.path());
        let counter = repository.path().join("spawn-count");
        let lsp = repository.path().join("counting-lsp.py");
        fs::write(&lsp, DUPLICATE_ACTIVATION_LSP).unwrap();
        let mut permissions = fs::metadata(&lsp).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&lsp, permissions).unwrap();

        let mut config = crate::config::LspServerConfig::rust_analyzer();
        config.command = lsp.display().to_string();
        config.heuristics = None;
        config.env = HashMap::from([(
            "MCPLS_SPAWN_COUNTER".to_string(),
            counter.display().to_string(),
        )]);
        let mut template_source = Translator::new()
            .with_extensions(HashMap::from([("rs".to_string(), "rust".to_string())]));
        template_source.set_lsp_configs(vec![config], Some(3));
        let registry =
            ProjectRegistry::with_translator_template(4, template_source.configuration_template());
        let project_id = ProjectId::new("repository").unwrap();
        let repository_identity = GitRepositoryIdentity::discover(repository.path())
            .unwrap()
            .unwrap();
        let linked_identity = GitRepositoryIdentity::discover(worktree.path())
            .unwrap()
            .unwrap();
        registry
            .add(
                ProjectIdentity::new(
                    project_id.clone(),
                    CanonicalRoot::new(repository.path()).unwrap(),
                )
                .with_repository_identity(repository_identity),
            )
            .await
            .unwrap();
        registry
            .add(
                ProjectIdentity::new(
                    project_id.clone(),
                    CanonicalRoot::new(worktree.path()).unwrap(),
                )
                .with_repository_identity(linked_identity),
            )
            .await
            .unwrap();

        let state = registry.activate(&project_id).await.unwrap();
        assert!(matches!(
            state.status(),
            ProjectStatus::Starting | ProjectStatus::Ready
        ));
        let ready = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if registry.status(&project_id).await.unwrap().status() == ProjectStatus::Ready {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await;
        assert!(
            ready.is_ok(),
            "linked-worktree project did not become ready"
        );
        assert_eq!(state.workspace_roots().len(), 2);
        assert_eq!(registry.actor_group_count(&project_id).await.unwrap(), 1);
        assert_eq!(fs::read_to_string(counter).unwrap(), "1");
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
