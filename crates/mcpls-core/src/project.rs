//! Project identity and canonical path routing primitives.

mod residency;

use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use ignore::WalkBuilder;
const CALL_HIERARCHY_PAGE_SIZE: usize = 64;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex, Notify, RwLock, broadcast, mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;
use tracing::Instrument as _;

use crate::bridge::DeferredResourceReference;
use crate::bridge::ast_grep::byte_offset_to_position;
use crate::bridge::convert_code_action_or_command;
use crate::bridge::lexical::{
    LexicalSearchBatchRequest, LexicalSearchBatchResult, LexicalSearchMatch, LexicalSearchRequest,
    LexicalSearchScan, collect_project_paths_filtered, find_matches,
};
use crate::bridge::resources::SourceResource;
use crate::bridge::resources::make_source_uri;
use crate::bridge::translator::SourceBudget;
use crate::bridge::translator::{
    CallHierarchyItemResult, DiagnosticOptions, SourceUnavailableReason, WorkspaceSymbol,
    page_items,
};
use crate::bridge::{
    ActivationHealth, CallHierarchyPrepareResult, CodeActionsResult, CompletionsResult,
    DefinitionResult, DiagnosticSeverity, DiagnosticsResult, DocumentSymbolOptions,
    DocumentSymbolPageRequest, DocumentSymbolsResult, FormatDocumentResult, HoverResult,
    IncomingCallsResult, InlayHintsResult, InspectSymbolBatchEntry, InspectSymbolBatchRequest,
    InspectSymbolBatchResult, InspectSymbolRequest, InspectSymbolResult, LocationsResult, LogEntry,
    LogLevel, OutgoingCallsResult, PositionEncoding, ProjectActivation, ProviderSynchronization,
    ReferencesResult, RenameResult, SemanticDiscoveryKind, SemanticDiscoveryResult,
    SemanticResultLimits, ServerCapability, ServerLogsResult, ServerMessage, ServerMessagesResult,
    SignatureHelpResult, SourceContext, SourceFrame, StructuralFileSnapshot, StructuralMatch,
    StructuralSearchResult, SymbolHandle, Translator, TranslatorTemplate, WillRenameFilesResult,
    WorkspaceSymbolBatchEntry, WorkspaceSymbolBatchRequest, WorkspaceSymbolBatchResult,
    WorkspaceSymbolMatchMode, WorkspaceSymbolPageRequest, WorkspaceSymbolResult,
    WorkspaceSymbolScope, path_to_uri, uri_to_path,
};
use crate::config::{EditSafetyConfig, ProjectConfig, ServerId};
use crate::edit_apply::{
    ApplyError, ApplyReport, apply_plan_with_documents, apply_plan_with_documents_and_backup,
};
use crate::edit_backup::BackupPolicy;
use crate::edit_coordinator::{EditCoordinator, EditLease};
use crate::edit_paths::{FileOperation, OperationValidationError, WorkspaceBoundary};
use crate::edit_plan::{AuditLogPolicy, EditAuditRecord, EditPlan, EditPlanStore, PlanId};
use crate::edit_preview::{
    EditProducer, PreviewArtifact, PreviewLimits, VerificationStatus, preview_workspace_edit,
    refresh_workspace_edit_documents,
};
use crate::lsp::{LspNotification, load_project_environment, resolve_command};
use crate::project_persistence::{PersistedProject, ProjectRegistrationStore};
use crate::rust_refactor::{logical_module_name, move_inline_module_preview_with_source};
use crate::workspace_edit::{EditOperation, normalize};
use lsp_types::WorkspaceEdit;
use residency::{RustGroupId, RustResidencyController, RustResidencyMode};

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

/// Why a project is currently dormant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectDormancyReason {
    /// The residency budget suspended an idle actor group.
    ResidencyEviction,
    /// The project was restored from persisted registration state.
    Restored,
}

impl ProjectDormancyReason {
    /// Return the stable wire spelling for this dormancy reason.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ResidencyEviction => "residency_eviction",
            Self::Restored => "restored",
        }
    }
}

/// Metadata describing the current dormant state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProjectDormancy {
    reason: ProjectDormancyReason,
    idle_for: Option<Duration>,
}

impl ProjectDormancy {
    const fn new(reason: ProjectDormancyReason, idle_for: Option<Duration>) -> Self {
        Self { reason, idle_for }
    }

    /// Return why the project became dormant.
    #[must_use]
    pub const fn reason(self) -> ProjectDormancyReason {
        self.reason
    }

    /// Return how long the evicted group had been idle, when known.
    #[must_use]
    pub const fn idle_for(self) -> Option<Duration> {
        self.idle_for
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
    truncated: bool,
    retention_floor: u64,
    next_sequence: u64,
}

impl ProjectEventSnapshot {
    /// Return events newer than the requested cursor.
    #[must_use]
    pub fn events(&self) -> &[ProjectEventRecord] {
        &self.events
    }

    /// Return the first retained sequence in this response, when any.
    #[must_use]
    pub fn first_sequence(&self) -> Option<u64> {
        self.events.first().map(ProjectEventRecord::sequence)
    }

    /// Return the last retained sequence in this response, when any.
    #[must_use]
    pub fn last_sequence(&self) -> Option<u64> {
        self.events.last().map(ProjectEventRecord::sequence)
    }

    /// Whether the requested cursor predates the retained bounded history.
    #[must_use]
    pub const fn resync_required(&self) -> bool {
        self.resync_required
    }

    /// Whether retained events remain after this page.
    #[must_use]
    pub const fn truncated(&self) -> bool {
        self.truncated
    }

    /// Return the latest cursor whose successor is entirely retained.
    #[must_use]
    pub const fn retention_floor(&self) -> u64 {
        self.retention_floor
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

    /// Return one retained immutable event record by sequence.
    #[must_use]
    pub fn record_at(&self, sequence: u64) -> Option<ProjectEventRecord> {
        self.records
            .iter()
            .find(|record| record.sequence == sequence)
            .cloned()
    }

    /// Return retained events newer than `cursor`, marking overflow when needed.
    #[must_use]
    pub fn snapshot_since(&self, cursor: Option<u64>, max_events: usize) -> ProjectEventSnapshot {
        let oldest = self
            .records
            .front()
            .map_or(self.next_sequence, |record| record.sequence);
        let resync_required = cursor.is_some_and(|cursor| cursor < oldest.saturating_sub(1));
        let mut records = self
            .records
            .iter()
            .filter(|record| cursor.is_none_or(|cursor| record.sequence > cursor))
            .cloned();
        let events = records.by_ref().take(max_events.max(1)).collect::<Vec<_>>();
        let truncated = records.next().is_some();
        let next_sequence = events.last().map_or_else(
            || cursor.unwrap_or(self.next_sequence.saturating_sub(1)),
            ProjectEventRecord::sequence,
        );
        ProjectEventSnapshot {
            events,
            resync_required,
            truncated,
            retention_floor: oldest.saturating_sub(1),
            next_sequence,
        }
    }
}

/// Observable project state, including the most recent failure detail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectState {
    status: ProjectStatus,
    last_error: Option<String>,
    dormancy: Option<ProjectDormancy>,
    runtime: ProjectRuntimeSummary,
}

impl ProjectState {
    const fn new(status: ProjectStatus, runtime: ProjectRuntimeSummary) -> Self {
        Self {
            status,
            last_error: None,
            dormancy: None,
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

    /// Return metadata for the current dormant state, when available.
    #[must_use]
    pub const fn dormancy(&self) -> Option<ProjectDormancy> {
        self.dormancy
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
            self.dormancy = state.dormancy;
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

struct ProjectRequestTiming {
    queued_at: Instant,
    span: tracing::Span,
}

impl ProjectRequestTiming {
    fn capture() -> Self {
        Self {
            queued_at: Instant::now(),
            span: tracing::Span::current(),
        }
    }
}

impl ProjectRequestSender {
    #[cfg(test)]
    fn new(sender: mpsc::Sender<ProjectRequest>) -> Self {
        Self::with_gate(sender, None, ProjectRequestGate::new())
    }

    #[cfg(test)]
    fn with_residency(sender: mpsc::Sender<ProjectRequest>, residency: ProjectResidency) -> Self {
        Self::with_gate(sender, Some(residency), ProjectRequestGate::new())
    }

    const fn with_gate(
        sender: mpsc::Sender<ProjectRequest>,
        residency: Option<ProjectResidency>,
        gate: ProjectRequestGate,
    ) -> Self {
        Self {
            sender,
            gate,
            residency,
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

    fn begin_shutdown(&self) {
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

        request = ProjectRequest::Timed {
            request: Box::new(request),
            timing: ProjectRequestTiming::capture(),
        };

        if let Some(mode) = request.rust_residency_mode()
            && let Some(residency) = &self.residency
        {
            request = match mode {
                RustResidencyMode::Touch => residency.touch_request(request),
                RustResidencyMode::Resume | RustResidencyMode::Activate => {
                    residency.resident_request(request, mode).await
                }
            };
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
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppliedEditPlan {
    /// Opaque identifier of the consumed plan.
    pub plan_id: PlanId,
    /// Human-readable operations captured by the preview.
    pub operations: Vec<String>,
    /// Unified diff captured by the preview.
    pub unified_diff: String,
    /// Complete immutable unified diff retained for bounded receipt resources.
    pub complete_unified_diff: String,
    /// Files replaced successfully.
    pub committed_files: Vec<PathBuf>,
    /// Optional semantic verification outcome for a specialized refactor.
    pub verification: Option<VerificationStatus>,
    /// Post-commit provider convergence results for workspace changes.
    pub provider_synchronization: Vec<ProviderSynchronization>,
}

/// A successful apply response that did not mutate the workspace yet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EditNotReady {
    /// Plan that remains valid for a same-plan retry.
    pub plan_id: PlanId,
    /// Caller-visible paths currently held by another edit.
    pub blocked_paths: Vec<PathBuf>,
    /// Suggested delay before retrying.
    pub retry_after_ms: u64,
}

/// A successful apply response whose immutable plan became stale.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EditConflict {
    /// Plan that must be previewed again.
    pub plan_id: PlanId,
    /// Paths whose preconditions no longer hold.
    pub changed_paths: Vec<PathBuf>,
    /// Stable reason code for clients.
    pub reason: String,
}

/// Expected and successful outcomes of a workspace-edit apply.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApplyEditPlanOutcome {
    /// The filesystem transaction committed.
    Applied(AppliedEditPlan),
    /// Another edit currently owns an overlapping path set.
    NotReady(EditNotReady),
    /// The plan's preconditions changed before it could commit.
    Conflict(EditConflict),
}

impl AppliedEditPlan {
    fn detail_json(&self) -> serde_json::Value {
        serde_json::json!({
            "plan_id": self.plan_id.as_str(),
            "operations": self.operations,
            "unified_diff": self.complete_unified_diff,
            "committed_files": self.committed_files,
            "verification": self.verification.map(VerificationStatus::as_str),
            "provider_synchronization": self.provider_synchronization.iter().map(|result| serde_json::json!({
                "provider": result.provider,
                "synchronized": result.synchronized,
                "watched_file_notifications": result.watched_file_notifications,
                "message": result.message,
            })).collect::<Vec<_>>(),
        })
    }

    fn estimated_bytes(&self) -> usize {
        self.complete_unified_diff.len()
            + self.unified_diff.len()
            + self.operations.iter().map(String::len).sum::<usize>()
            + self
                .provider_synchronization
                .iter()
                .map(|result| result.provider.len() + result.message.as_deref().map_or(0, str::len))
                .sum::<usize>()
    }

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
    /// Exact source snapshots containing those matches, listed once per file.
    pub(crate) matched_files: Vec<StructuralMatchedFile>,
    /// Whether only parser validation was requested.
    pub(crate) parse_only: bool,
}

/// Snapshot metadata needed to fetch source context without repeating file paths per match.
#[derive(Debug, Clone)]
pub(crate) struct StructuralMatchedFile {
    pub(crate) path: PathBuf,
    pub(crate) content_hash: String,
    pub(crate) document_version: Option<i32>,
    pub(crate) total_bytes: usize,
    pub(crate) total_lines: u32,
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

/// One LSP edit request that must be generated and snapshotted atomically.
pub(crate) enum GeneratedEditRequest {
    Rename {
        file_path: String,
        line: u32,
        character: u32,
        new_name: String,
    },
    Format {
        file_path: String,
        tab_size: u32,
        insert_spaces: bool,
    },
    RangeFormat {
        file_path: String,
        start: (u32, u32),
        end: (u32, u32),
        tab_size: u32,
        insert_spaces: bool,
    },
    MoveItem {
        file_path: String,
        start: (u32, u32),
        end: (u32, u32),
        direction: String,
    },
}

pub(crate) struct GeneratedEditPreview {
    pub(crate) supported: bool,
    pub(crate) artifact: Option<PreviewArtifact>,
}

/// Coordinate target recovered from an actor-owned snapshot handle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedSymbolTarget {
    pub(crate) file_path: String,
    pub(crate) line: u32,
    pub(crate) character: u32,
}

enum ProjectRequest {
    Timed {
        request: Box<Self>,
        timing: ProjectRequestTiming,
    },
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
        limits: SemanticResultLimits,
        page_offset: Option<usize>,
        reply: oneshot::Sender<Result<ReferencesResult, String>>,
    },
    ReadSourceResource {
        resource: SourceResource,
        max_response_bytes: usize,
        reply: oneshot::Sender<Result<SourceFrame, String>>,
    },
    ResolveSymbolHandle {
        symbol_handle: SymbolHandle,
        reply: oneshot::Sender<Result<ResolvedSymbolTarget, String>>,
    },
    Diagnostics {
        file_path: String,
        options: DiagnosticOptions,
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
        request: DocumentSymbolPageRequest,
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
    GeneratedEditPreview {
        project_id: String,
        request: GeneratedEditRequest,
        encoding: PositionEncoding,
        root: PathBuf,
        reply: oneshot::Sender<Result<GeneratedEditPreview, String>>,
    },
    SemanticDiscovery {
        file_path: String,
        line: u32,
        character: u32,
        kind: SemanticDiscoveryKind,
        reply: oneshot::Sender<Result<SemanticDiscoveryResult, String>>,
    },
    WorkspaceSymbol {
        request: WorkspaceSymbolPageRequest,
        reply: oneshot::Sender<Result<WorkspaceSymbolResult, String>>,
    },
    WorkspaceSymbolBatch {
        request: WorkspaceSymbolBatchRequest,
        reply: oneshot::Sender<Result<WorkspaceSymbolBatchResult, String>>,
    },
    LexicalSearch {
        request: LexicalSearchRequest,
        reply: oneshot::Sender<Result<LexicalSearchScan, String>>,
    },
    LexicalSearchBatch {
        request: LexicalSearchBatchRequest,
        reply: oneshot::Sender<Result<LexicalSearchBatchResult, String>>,
    },
    InspectSymbol {
        request: InspectSymbolRequest,
        reply: oneshot::Sender<Result<InspectSymbolResult, String>>,
    },
    InspectSymbolBatch {
        request: Box<InspectSymbolBatchRequest>,
        reply: oneshot::Sender<Result<InspectSymbolBatchResult, String>>,
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
        page_token: Option<String>,
        reply: oneshot::Sender<Result<CallHierarchyPrepareResult, String>>,
    },
    IncomingCalls {
        item: serde_json::Value,
        limits: SemanticResultLimits,
        reply: oneshot::Sender<Result<IncomingCallsResult, String>>,
    },
    OutgoingCalls {
        item: serde_json::Value,
        limits: SemanticResultLimits,
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
        options: DiagnosticOptions,
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
    SourcePathAuthorized {
        path: PathBuf,
        reply: oneshot::Sender<bool>,
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
    InspectEditPlan {
        plan_id: PlanId,
        project_id: String,
        reply: oneshot::Sender<Result<crate::edit_plan::EditPlanApprovalSummary, String>>,
    },
    ReadEditPlanDiff {
        plan_id: PlanId,
        project_id: String,
        reply: oneshot::Sender<Result<String, String>>,
    },
    ReadAppliedEditDetail {
        plan_id: PlanId,
        project_id: String,
        reply: oneshot::Sender<Result<String, String>>,
    },
    ApplyEditPlan {
        plan_id: PlanId,
        project_id: String,
        root: PathBuf,
        session_id: Option<String>,
        principal: Option<String>,
        lease: EditLease,
        reply: oneshot::Sender<Result<ApplyEditPlanOutcome, String>>,
    },
    FinalizeEditPlan {
        prepared: Box<PreparedEditPlan>,
        result: Result<ApplyReport, ApplyError>,
        reply: oneshot::Sender<Result<ApplyEditPlanOutcome, String>>,
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
        dormancy: ProjectDormancy,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RustResidencyRequirement {
    None,
    Touch,
    Resume,
    Activate,
}

impl ProjectRequest {
    fn into_resident(self) -> (Self, Option<residency::RustResidencyGuard>) {
        match self {
            Self::Resident { request, guard } => (*request, Some(guard)),
            request => (request, None),
        }
    }

    fn into_timed(self) -> (Self, ProjectRequestTiming) {
        match self {
            Self::Timed { request, timing } => (*request, timing),
            request => (
                request,
                ProjectRequestTiming {
                    queued_at: Instant::now(),
                    span: tracing::Span::none(),
                },
            ),
        }
    }

    const fn rust_residency_requirement(&self) -> RustResidencyRequirement {
        if let Self::Timed { request, .. } = self {
            return request.rust_residency_requirement();
        }
        if matches!(
            self,
            Self::Activate { .. }
                | Self::ActivateWorkspaceRoots { .. }
                | Self::AddWorkspaceRoot { .. }
                | Self::Restart { .. }
        ) {
            return RustResidencyRequirement::Activate;
        }
        if matches!(
            self,
            Self::Hover { .. }
                | Self::Definition { .. }
                | Self::References { .. }
                | Self::ResolveSymbolHandle { .. }
                | Self::Diagnostics { .. }
                | Self::Rename { .. }
                | Self::RenameWorkspaceEdit { .. }
                | Self::Completions { .. }
                | Self::DocumentSymbols { .. }
                | Self::FormatDocument { .. }
                | Self::FormatWorkspaceEdit { .. }
                | Self::SemanticDiscovery { .. }
                | Self::WorkspaceSymbol { .. }
                | Self::WorkspaceSymbolBatch { .. }
                | Self::InspectSymbol { .. }
                | Self::InspectSymbolBatch { .. }
                | Self::CodeActions { .. }
                | Self::CodeActionList { .. }
                | Self::PrepareCallHierarchy { .. }
                | Self::IncomingCalls { .. }
                | Self::OutgoingCalls { .. }
                | Self::SignatureHelp { .. }
                | Self::InlayHints { .. }
                | Self::GoToImplementation { .. }
                | Self::GoToTypeDefinition { .. }
                | Self::ApplyEditPlan { .. }
                | Self::MoveInlineModulePreview { .. }
                | Self::PathRenamePreview { .. }
                | Self::GeneratedEditPreview { .. }
        ) || matches!(
            self,
            Self::StructuralReplacePreview {
                request: StructuralReplaceRequest {
                    dialect: StructuralDialect::RustAnalyzerSsr,
                    ..
                },
                ..
            }
        ) {
            RustResidencyRequirement::Resume
        } else if matches!(
            self,
            Self::Resident { .. }
                | Self::SetStatus { .. }
                | Self::Suspend { .. }
                | Self::ServerExited { .. }
                | Self::Shutdown { .. }
                | Self::Fail { .. }
                | Self::FinalizeEditPlan { .. }
        ) {
            RustResidencyRequirement::None
        } else {
            RustResidencyRequirement::Touch
        }
    }

    const fn rust_residency_mode(&self) -> Option<RustResidencyMode> {
        match self.rust_residency_requirement() {
            RustResidencyRequirement::None => None,
            RustResidencyRequirement::Touch => Some(RustResidencyMode::Touch),
            RustResidencyRequirement::Resume => Some(RustResidencyMode::Resume),
            RustResidencyRequirement::Activate => Some(RustResidencyMode::Activate),
        }
    }

    const fn resumes_rust_runtime(&self) -> bool {
        matches!(
            self.rust_residency_requirement(),
            RustResidencyRequirement::Resume
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
            Self::ResolveSymbolHandle { reply, .. } => reject!(reply),
            Self::Diagnostics { reply, .. } => reject!(reply),
            Self::Rename { reply, .. } => reject!(reply),
            Self::RenameWorkspaceEdit { reply, .. } | Self::FormatWorkspaceEdit { reply, .. } => {
                reject!(reply)
            }
            Self::GeneratedEditPreview { reply, .. } => reject!(reply),
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
            Self::Timed { request, .. } => request.is_cancelled(),
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
            Self::ReadSourceResource { reply, .. } => reply.is_closed(),
            Self::ResolveSymbolHandle { reply, .. } => reply.is_closed(),
            Self::Diagnostics { reply, .. } | Self::CachedDiagnostics { reply, .. } => {
                reply.is_closed()
            }
            Self::Rename { reply, .. } => reply.is_closed(),
            Self::RenameWorkspaceEdit { reply, .. } | Self::FormatWorkspaceEdit { reply, .. } => {
                reply.is_closed()
            }
            Self::GeneratedEditPreview { reply, .. } => reply.is_closed(),
            Self::SemanticDiscovery { reply, .. } => reply.is_closed(),
            Self::Completions { reply, .. } => reply.is_closed(),
            Self::DocumentSymbols { reply, .. } => reply.is_closed(),
            Self::FormatDocument { reply, .. } => reply.is_closed(),
            Self::WorkspaceSymbol { reply, .. } => reply.is_closed(),
            Self::WorkspaceSymbolBatch { reply, .. } => reply.is_closed(),
            Self::LexicalSearch { reply, .. } => reply.is_closed(),
            Self::LexicalSearchBatch { reply, .. } => reply.is_closed(),
            Self::InspectSymbol { reply, .. } => reply.is_closed(),
            Self::InspectSymbolBatch { reply, .. } => reply.is_closed(),
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
            Self::SourcePathAuthorized { reply, .. } => reply.is_closed(),
            Self::AddWorkspaceRoot { reply, .. } => reply.is_closed(),
            Self::TakeEditPlan { reply, .. } => reply.is_closed(),
            Self::InspectEditPlan { reply, .. } => reply.is_closed(),
            Self::ReadEditPlanDiff { reply, .. } => reply.is_closed(),
            Self::ReadAppliedEditDetail { reply, .. } => reply.is_closed(),
            Self::ApplyEditPlan { reply, .. } => reply.is_closed(),
            Self::ServerLogs { reply, .. } => reply.is_closed(),
            Self::ServerMessages { reply, .. } => reply.is_closed(),
            Self::ServerCapabilities { reply, .. } => reply.is_closed(),
            Self::PublishEvent { .. }
            | Self::Shutdown { .. }
            | Self::Suspend { .. }
            | Self::Notification { .. }
            | Self::ServerExited { .. }
            | Self::FinalizeEditPlan { .. } => false,
        }
    }
}

/// Cloneable handle for querying and controlling one project actor.
#[derive(Clone)]
pub struct ProjectHandle {
    sender: ProjectRequestSender,
    status: watch::Receiver<ProjectStatus>,
    state: watch::Receiver<ProjectState>,
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

    fn state_snapshot(&self) -> ProjectState {
        self.state.borrow().clone()
    }

    /// Subscribe to typed project lifecycle and failure events.
    #[must_use]
    pub fn subscribe_events(&self) -> broadcast::Receiver<ProjectEvent> {
        self.events.subscribe()
    }

    /// Return retained project events newer than an optional polling cursor.
    #[must_use]
    pub fn event_snapshot(&self, cursor: Option<u64>, max_events: usize) -> ProjectEventSnapshot {
        self.event_history
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .snapshot_since(cursor, max_events)
    }

    /// Return one retained immutable event record by sequence.
    #[must_use]
    pub fn event_record(&self, sequence: u64) -> Option<ProjectEventRecord> {
        self.event_history
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .record_at(sequence)
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
        limits: SemanticResultLimits,
    ) -> Result<ReferencesResult, ProjectActorError> {
        self.references_with_cursor(
            file_path,
            line,
            character,
            include_declaration,
            limits,
            None,
        )
        .await
    }

    /// Route one deterministic reference page through this project's actor.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor is closed, cancels the response, or the
    /// actor-owned translator rejects the request.
    pub async fn references_with_cursor(
        &self,
        file_path: String,
        line: u32,
        character: u32,
        include_declaration: bool,
        limits: SemanticResultLimits,
        page_offset: Option<usize>,
    ) -> Result<ReferencesResult, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::References {
                file_path,
                line,
                character,
                include_declaration,
                limits,
                page_offset,
                reply,
            })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    /// Read a snapshot-bound source context resource.
    pub(crate) async fn read_source_resource(
        &self,
        resource: SourceResource,
        max_response_bytes: usize,
    ) -> Result<SourceFrame, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::ReadSourceResource {
                resource,
                max_response_bytes,
                reply,
            })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    /// Resolve a snapshot-bound symbol handle and find its references.
    ///
    /// # Errors
    ///
    /// Returns a typed operation error when the handle is unknown, expired,
    /// belongs to another project actor, or its source snapshot is stale.
    pub(crate) async fn resolve_symbol_handle(
        &self,
        symbol_handle: SymbolHandle,
    ) -> Result<ResolvedSymbolTarget, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::ResolveSymbolHandle {
                symbol_handle,
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
        self.diagnostics_with_options(file_path, DiagnosticOptions::default())
            .await
    }

    /// Route a bounded, filtered diagnostics request through the project actor.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor closes, cancels, or rejects the request.
    pub async fn diagnostics_with_options(
        &self,
        file_path: String,
        options: DiagnosticOptions,
    ) -> Result<DiagnosticsResult, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::Diagnostics {
                file_path,
                options,
                reply,
            })
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
        options: DocumentSymbolOptions,
    ) -> Result<DocumentSymbolsResult, ProjectActorError> {
        self.document_symbol_page(DocumentSymbolPageRequest {
            file_path,
            options,
            max_bytes: 16 * 1024,
            page_token: None,
        })
        .await
    }

    /// Route one bounded document-symbol page through this project's actor.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor closes, the caller cancels, or outline
    /// generation or continuation validation fails.
    pub async fn document_symbol_page(
        &self,
        request: DocumentSymbolPageRequest,
    ) -> Result<DocumentSymbolsResult, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::DocumentSymbols { request, reply })
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

    /// Generate an LSP edit and snapshot its source in one actor request.
    pub(crate) async fn preview_generated_edit(
        &self,
        project_id: String,
        request: GeneratedEditRequest,
        encoding: PositionEncoding,
        root: PathBuf,
    ) -> Result<GeneratedEditPreview, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::GeneratedEditPreview {
                project_id,
                request,
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

    /// Route a bounded workspace-symbol page through this project's actor-owned translator.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor is closed, cancels the response, or the
    /// actor-owned translator rejects the request.
    pub async fn workspace_symbol(
        &self,
        request: WorkspaceSymbolPageRequest,
    ) -> Result<WorkspaceSymbolResult, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::WorkspaceSymbol { request, reply })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    /// Route a bounded workspace-symbol batch through one project actor request.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor closes, the caller cancels, or any
    /// downstream workspace-symbol request fails.
    pub async fn workspace_symbol_batch(
        &self,
        request: WorkspaceSymbolBatchRequest,
    ) -> Result<WorkspaceSymbolBatchResult, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::WorkspaceSymbolBatch { request, reply })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    /// Search project snapshots with bounded lexical matching.
    pub(crate) async fn lexical_search(
        &self,
        request: LexicalSearchRequest,
    ) -> Result<LexicalSearchScan, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::LexicalSearch { request, reply })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    /// Search several lexical queries from one actor-owned source snapshot pass.
    pub(crate) async fn lexical_search_batch(
        &self,
        request: LexicalSearchBatchRequest,
    ) -> Result<LexicalSearchBatchResult, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::LexicalSearchBatch { request, reply })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    /// Resolve and inspect one symbol in a single actor-owned snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor closes, the caller cancels, or symbol
    /// resolution fails.
    pub async fn inspect_symbol(
        &self,
        request: InspectSymbolRequest,
    ) -> Result<InspectSymbolResult, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::InspectSymbol { request, reply })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response
            .await
            .map_err(|_| ProjectActorError::Cancelled)?
            .map_err(ProjectActorError::Operation)
    }

    /// Inspect several symbols concurrently through one actor request.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor closes, the caller cancels, or symbol
    /// resolution fails.
    pub async fn inspect_symbol_batch(
        &self,
        request: InspectSymbolBatchRequest,
    ) -> Result<InspectSymbolBatchResult, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::InspectSymbolBatch {
                request: Box::new(request),
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
        page_token: Option<String>,
    ) -> Result<CallHierarchyPrepareResult, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::PrepareCallHierarchy {
                file_path,
                line,
                character,
                page_token,
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
        limits: SemanticResultLimits,
    ) -> Result<IncomingCallsResult, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::IncomingCalls {
                item,
                limits,
                reply,
            })
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
        limits: SemanticResultLimits,
    ) -> Result<OutgoingCallsResult, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::OutgoingCalls {
                item,
                limits,
                reply,
            })
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
        self.cached_diagnostics_with_options(file_path, DiagnosticOptions::default())
            .await
    }

    /// Route bounded, filtered cached diagnostics through this project actor.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor closes, cancels, or rejects the request.
    pub async fn cached_diagnostics_with_options(
        &self,
        file_path: String,
        options: DiagnosticOptions,
    ) -> Result<DiagnosticsResult, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::CachedDiagnostics {
                file_path,
                options,
                reply,
            })
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

    /// Whether this actor has an active-LSP source-read capability for `path`.
    pub async fn source_path_is_authorized(
        &self,
        path: PathBuf,
    ) -> Result<bool, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::SourcePathAuthorized { path, reply })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response.await.map_err(|_| ProjectActorError::Cancelled)
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

    /// Inspect one project-owned edit plan without consuming it.
    pub(crate) async fn inspect_edit_plan(
        &self,
        plan_id: PlanId,
        project_id: String,
    ) -> Result<crate::edit_plan::EditPlanApprovalSummary, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::InspectEditPlan {
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

    /// Read the complete immutable unified diff for one retained edit plan.
    pub(crate) async fn read_edit_plan_diff(
        &self,
        plan_id: PlanId,
        project_id: String,
    ) -> Result<String, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::ReadEditPlanDiff {
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

    /// Read the complete immutable result for one committed edit plan.
    pub(crate) async fn read_applied_edit_detail(
        &self,
        plan_id: PlanId,
        project_id: String,
    ) -> Result<String, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::ReadAppliedEditDetail {
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

    /// Apply a plan while holding the registry-owned path reservation.
    pub(crate) async fn apply_edit_plan_with_lease(
        &self,
        plan_id: PlanId,
        project_id: String,
        root: PathBuf,
        session_id: Option<String>,
        principal: Option<String>,
        lease: EditLease,
    ) -> Result<ApplyEditPlanOutcome, ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::ApplyEditPlan {
                plan_id,
                project_id,
                root,
                session_id,
                principal,
                lease,
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
        self.sender.begin_shutdown();
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
    symbol_handles: std::sync::Mutex<SymbolHandleStore>,
    workspace_symbol_results: std::sync::Mutex<HashMap<String, WorkspaceSymbolResult>>,
    inspect_symbol_batch_pages: std::sync::Mutex<InspectSymbolBatchPageStore>,
    deferred_results: std::sync::Arc<std::sync::Mutex<DeferredResultStore>>,
    deferred_scope: Option<String>,
    inline_module_checks: HashMap<PlanId, InlineModuleSemanticCheck>,
    applied_edit_receipts: VecDeque<AppliedEditPlan>,
    applied_edit_receipt_bytes: usize,
    edit_conflicts: VecDeque<EditConflict>,
    active_edit_workers: usize,
    activation_health: ActivationHealth,
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

struct PreparedEditPlan {
    plan: EditPlan,
    boundary: WorkspaceBoundary,
    backup_policy: Option<BackupPolicy>,
    semantic_check: Option<InlineModuleSemanticCheck>,
    resource_operations: Vec<FileOperation>,
    text_changes: Vec<(PathBuf, String)>,
    open_documents: Vec<(PathBuf, i32, String)>,
    audit: EditAuditRecord,
    documents: std::sync::Arc<crate::bridge::DocumentTracker>,
    lease: EditLease,
}

enum PreparedEditResult {
    AlreadyApplied(AppliedEditPlan),
    AlreadyConflicted(EditConflict),
    Ready(Box<PreparedEditPlan>),
}

const LANGUAGE_SERVER_EXITED: &str = "language server exited";
const MAX_AUTOMATIC_RESTART_ATTEMPTS: usize = 3;
const MAX_INLINE_MODULE_CHECKS: usize = 256;
const MAX_EDIT_ADMISSION_WAIT: Duration = Duration::from_secs(5);
const MAX_APPLIED_EDIT_RECEIPTS: usize = 256;
const MAX_APPLIED_EDIT_RECEIPT_BYTES: usize = 32 * 1024 * 1024;
const DEFAULT_RUST_RESIDENCY_LIMIT: usize = 5;
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

#[derive(Debug, Clone, PartialEq, Eq)]
enum SourceSnapshot {
    Version(i32),
    Hash(String),
}

#[derive(Debug, Clone)]
struct StoredSymbolTarget {
    file_path: PathBuf,
    line: u32,
    character: u32,
    snapshot: SourceSnapshot,
    created_at: Instant,
}

impl StoredSymbolTarget {
    fn new(file_path: PathBuf, line: u32, character: u32, snapshot: SourceSnapshot) -> Self {
        Self {
            file_path,
            line,
            character,
            snapshot,
            created_at: Instant::now(),
        }
    }
}

struct SymbolHandleStore {
    entries: HashMap<SymbolHandle, StoredSymbolTarget>,
    ttl: Duration,
    max_entries: usize,
}

/// Deferred payload and the immutable snapshot that produced it.
#[derive(Debug, Clone)]
pub(crate) struct DeferredResourcePayload {
    pub value: serde_json::Value,
    pub snapshot_hash: String,
}

struct StoredDeferredResult {
    value: serde_json::Value,
    snapshot_hash: String,
    created_at: Instant,
    scope: String,
}

struct DeferredResultStore {
    entries: HashMap<String, StoredDeferredResult>,
    ttl: Duration,
    max_entries: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LexicalSearchPageState {
    matches: Vec<LexicalSearchMatch>,
    total_matches: usize,
    scanned_files: usize,
    scanned_bytes: usize,
    snapshot_identity: String,
    request_identity: String,
}

struct LexicalFileSnapshot {
    path: PathBuf,
    document_version: Option<i32>,
    content_hash: String,
    source: String,
    project_relative_path: String,
}

fn lexical_search_request_identity(request: &LexicalSearchRequest) -> String {
    let value = (
        &request.query,
        request.mode,
        request.case,
        request.multiline,
        request.max_files,
        request.include_generated,
        &request.include_paths,
        &request.exclude_paths,
        request.context_lines,
    );
    format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&value).unwrap_or_default())
    )
}

pub(crate) fn lexical_page_cursor(token: &str, offset: usize) -> String {
    format!("mcpls-deferred:///{token}?offset={offset:020}")
}

fn parse_lexical_page_cursor(cursor: &str) -> Result<(&str, usize), String> {
    let cursor = cursor.strip_prefix("mcpls-deferred:///").ok_or_else(|| {
        "page_token must be the next_cursor returned by lexical_search".to_owned()
    })?;
    let (token, offset) = cursor
        .split_once("?offset=")
        .ok_or_else(|| "invalid lexical_search page_token".to_owned())?;
    if token.is_empty() {
        return Err("invalid lexical_search page_token".to_owned());
    }
    let offset = offset
        .parse::<usize>()
        .map_err(|_| "invalid lexical_search page_token offset".to_owned())?;
    Ok((token, offset))
}

#[derive(Clone)]
struct InspectSymbolBatchSnapshot {
    entries: Vec<InspectSymbolBatchEntry>,
    inspections_started: usize,
    snapshot_identity: String,
    truncated: bool,
    max_items: usize,
}

struct StoredInspectSymbolBatchSnapshot {
    snapshot: InspectSymbolBatchSnapshot,
    scope: String,
    created_at: Instant,
}

struct InspectSymbolBatchPageStore {
    entries: HashMap<String, StoredInspectSymbolBatchSnapshot>,
    ttl: Duration,
    max_entries: usize,
}

impl InspectSymbolBatchPageStore {
    fn new() -> Self {
        Self {
            entries: HashMap::new(),
            ttl: Duration::from_secs(15 * 60),
            max_entries: 64,
        }
    }

    fn prune(&mut self) {
        let now = Instant::now();
        self.entries
            .retain(|_, entry| now.duration_since(entry.created_at) < self.ttl);
    }

    fn insert(&mut self, snapshot: InspectSymbolBatchSnapshot, scope: &str) -> String {
        self.prune();
        while self.entries.len() >= self.max_entries {
            let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.created_at)
                .map(|(token, _)| token.clone())
            else {
                break;
            };
            self.entries.remove(&oldest);
        }
        let token = uuid::Uuid::new_v4().to_string();
        self.entries.insert(
            token.clone(),
            StoredInspectSymbolBatchSnapshot {
                snapshot,
                scope: scope.to_owned(),
                created_at: Instant::now(),
            },
        );
        token
    }

    fn read(&mut self, token: &str, scope: &str) -> Result<InspectSymbolBatchSnapshot, String> {
        self.prune();
        self.entries
            .get(token)
            .filter(|entry| entry.scope == scope)
            .map(|entry| entry.snapshot.clone())
            .ok_or_else(|| "stale_resource: inspect batch page is missing or expired".to_owned())
    }

    fn remove(&mut self, token: &str) {
        self.entries.remove(token);
    }
}

fn inspect_symbol_batch_cursor(token: &str, offset: usize) -> String {
    format!("mcpls-inspect-batch:///{token}?offset={offset:020}")
}

fn parse_inspect_symbol_batch_cursor(cursor: &str) -> Result<(&str, usize), String> {
    let cursor = cursor
        .strip_prefix("mcpls-inspect-batch:///")
        .ok_or_else(|| {
            "page_token must be the next_cursor returned by inspect_symbol_batch".to_owned()
        })?;
    let (token, offset) = cursor
        .split_once("?offset=")
        .ok_or_else(|| "invalid inspect batch page_token".to_owned())?;
    if token.is_empty() {
        return Err("invalid inspect batch page_token".to_owned());
    }
    let offset = offset
        .parse::<usize>()
        .map_err(|_| "invalid inspect batch page_token offset".to_owned())?;
    Ok((token, offset))
}

fn update_inspect_symbol_batch_page_metadata(
    result: &mut InspectSymbolBatchResult,
    snapshot: &InspectSymbolBatchSnapshot,
    token: &str,
    offset: usize,
) {
    result.returned_targets = result.entries.len();
    result.remaining_targets = snapshot
        .entries
        .len()
        .saturating_sub(offset + result.returned_targets);
    result.next_cursor = (result.remaining_targets > 0)
        .then(|| inspect_symbol_batch_cursor(token, offset + result.returned_targets));
    result.returned_items = result
        .entries
        .iter()
        .filter_map(|entry| entry.result.as_ref())
        .map(|entry| entry.sections.returned_items())
        .sum();
    result.truncated = snapshot.truncated || result.remaining_targets > 0;
    result.returned_bytes = 0;
    for _ in 0..4 {
        let returned_bytes = serde_json::to_vec(result).map_or(usize::MAX, |json| json.len());
        if result.returned_bytes == returned_bytes {
            break;
        }
        result.returned_bytes = returned_bytes;
    }
}

fn update_inspect_symbol_byte_count(result: &mut InspectSymbolResult) {
    // `returned_bytes` is part of the serialized result, so assigning it
    // once under-reports whenever its digit count changes the payload.
    for _ in 0..4 {
        let serialized_bytes = serde_json::to_vec(result).map_or(0, |json| json.len());
        if result.returned_bytes == serialized_bytes {
            break;
        }
        result.returned_bytes = serialized_bytes;
    }
}

fn bounded_inspect_symbol_batch_page(
    snapshot: &InspectSymbolBatchSnapshot,
    token: &str,
    offset: usize,
) -> Result<InspectSymbolBatchResult, String> {
    if offset > snapshot.entries.len() {
        return Err("inspect batch page_token offset is outside the retained result".to_owned());
    }
    let max_bytes = crate::bridge::translator::INSPECT_SYMBOL_RESULT_MAX_BYTES;
    let mut result = InspectSymbolBatchResult {
        entries: Vec::new(),
        inspections_started: snapshot.inspections_started,
        total_targets: snapshot.entries.len(),
        returned_targets: 0,
        remaining_targets: snapshot.entries.len().saturating_sub(offset),
        next_cursor: None,
        snapshot_identity: snapshot.snapshot_identity.clone(),
        returned_items: 0,
        budget: crate::bridge::InspectSymbolBudget {
            max_bytes,
            max_items: snapshot.max_items,
        },
        returned_bytes: 0,
        truncated: snapshot.truncated,
    };

    for entry in snapshot.entries.iter().skip(offset) {
        result.entries.push(entry.clone());
        update_inspect_symbol_batch_page_metadata(&mut result, snapshot, token, offset);
        if serde_json::to_vec(&result).map_or(usize::MAX, |json| json.len()) > max_bytes {
            result.entries.pop();
            update_inspect_symbol_batch_page_metadata(&mut result, snapshot, token, offset);
            break;
        }
    }

    if result.entries.is_empty() && offset < snapshot.entries.len() {
        return Err("inspect batch target metadata exceeds the response page budget".to_owned());
    }
    debug_assert!(serde_json::to_vec(&result).is_ok_and(|json| json.len() <= max_bytes));
    Ok(result)
}

impl DeferredResultStore {
    fn new() -> Self {
        Self {
            entries: HashMap::new(),
            ttl: Duration::from_secs(15 * 60),
            max_entries: 128,
        }
    }

    fn prune(&mut self) {
        let now = Instant::now();
        self.entries
            .retain(|_, result| now.duration_since(result.created_at) < self.ttl);
    }

    fn insert_scoped(
        &mut self,
        value: serde_json::Value,
        snapshot_hash: String,
        scope: &str,
    ) -> DeferredResourceReference {
        self.insert_scoped_kind(value, snapshot_hash, scope, "inspect_symbol_section")
    }

    fn insert_scoped_kind(
        &mut self,
        value: serde_json::Value,
        snapshot_hash: String,
        scope: &str,
        kind: &str,
    ) -> DeferredResourceReference {
        self.prune();
        while self.entries.len() >= self.max_entries {
            let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, result)| result.created_at)
                .map(|(token, _)| token.clone())
            else {
                break;
            };
            self.entries.remove(&oldest);
        }
        let token = uuid::Uuid::new_v4().to_string();
        let total_bytes = serde_json::to_vec(&value).ok().map(|json| json.len());
        self.entries.insert(
            token.clone(),
            StoredDeferredResult {
                value,
                snapshot_hash: snapshot_hash.clone(),
                created_at: Instant::now(),
                scope: scope.to_owned(),
            },
        );
        DeferredResourceReference {
            uri: format!("mcpls-deferred:///{token}"),
            kind: kind.to_owned(),
            snapshot_hash,
            document_version: None,
            total_bytes,
        }
    }

    fn invalidate_scope(&mut self, scope: &str) {
        self.entries.retain(|_, result| result.scope != scope);
    }

    fn read(&mut self, token: &str) -> Result<DeferredResourcePayload, String> {
        self.read_entry(token, None)
            .map(|result| DeferredResourcePayload {
                value: result.value.clone(),
                snapshot_hash: result.snapshot_hash.clone(),
            })
    }

    fn read_scoped(&mut self, token: &str, scope: &str) -> Result<serde_json::Value, String> {
        self.read_entry(token, Some(scope))
            .map(|result| result.value.clone())
    }

    fn read_entry(
        &mut self,
        token: &str,
        scope: Option<&str>,
    ) -> Result<&StoredDeferredResult, String> {
        self.prune();
        self.entries
            .get(token)
            .filter(|result| scope.is_none_or(|scope| result.scope == scope))
            .ok_or_else(|| "stale_resource: deferred result is missing or expired".to_owned())
    }
}

fn attach_document_symbol_handles(
    store: &mut SymbolHandleStore,
    symbols: &mut [crate::bridge::Symbol],
    path: &Path,
    snapshot: &SourceSnapshot,
    parent: Option<&SymbolHandle>,
) {
    for symbol in symbols {
        symbol.parent_symbol_handle = parent.cloned();
        let handle = store.insert(StoredSymbolTarget::new(
            path.to_path_buf(),
            symbol.selection_range.start.line,
            symbol.selection_range.start.character,
            snapshot.clone(),
        ));
        symbol.symbol_handle = Some(handle.clone());
        if let Some(children) = &mut symbol.children {
            attach_document_symbol_handles(store, children, path, snapshot, Some(&handle));
        }
    }
}

fn rendered_workspace_symbol_position(
    rendered: &str,
    name: &str,
    range: &crate::bridge::Range,
) -> Option<(u32, u32)> {
    (!name.is_empty()).then_some(())?;

    rendered.lines().find_map(|rendered_line| {
        let (number, text) = rendered_line.split_once(" | ")?;
        let line = number.trim().parse::<u32>().ok()?;
        if !(range.start.line..=range.end.line).contains(&line) {
            return None;
        }
        symbol_position_in_line(line, text, name)
    })
}

fn source_symbol_position(
    source: &str,
    name: &str,
    range: &crate::bridge::Range,
) -> Option<(u32, u32)> {
    source.lines().enumerate().find_map(|(index, text)| {
        let line = u32::try_from(index).ok()?.saturating_add(1);
        (range.start.line..=range.end.line)
            .contains(&line)
            .then(|| symbol_position_in_line(line, text, name))
            .flatten()
    })
}

fn symbol_position_in_line(line: u32, text: &str, name: &str) -> Option<(u32, u32)> {
    (!name.is_empty()).then_some(())?;
    let text = text.trim_start();
    if text.starts_with("//") || text.starts_with("#") {
        return None;
    }
    text.match_indices(name).find_map(|(offset, _)| {
        let before = text[..offset].chars().next_back();
        let after = text[offset + name.len()..].chars().next();
        if before.is_none_or(|character| !character.is_alphanumeric() && character != '_')
            && after.is_none_or(|character| !character.is_alphanumeric() && character != '_')
        {
            Some((
                line,
                u32::try_from(text[..offset].encode_utf16().count()).ok()? + 1,
            ))
        } else {
            None
        }
    })
}

fn rendered_struct_declaration(rendered: &str, name: &str, range: &crate::bridge::Range) -> bool {
    rendered.lines().any(|rendered_line| {
        let Some((number, text)) = rendered_line.split_once(" | ") else {
            return false;
        };
        let Ok(line) = number.trim().parse::<u32>() else {
            return false;
        };
        if line != range.start.line {
            return false;
        }
        let text = text.trim_start();
        let Some((prefix, _)) = text.split_once(name) else {
            return false;
        };
        prefix.split_whitespace().next_back() == Some("struct")
    })
}

fn discard_workspace_symbol_struct_uses(symbols: &mut Vec<WorkspaceSymbol>) {
    let declarations = symbols
        .iter()
        .filter(|symbol| {
            symbol.kind == "Struct"
                && matches!(
                    &symbol.location.source,
                    SourceContext::Available(frame)
                        if rendered_struct_declaration(&frame.text, &symbol.name, &symbol.location.range)
                )
        })
        .map(|symbol| (symbol.name.clone(), symbol.location.uri.clone()))
        .collect::<HashSet<_>>();
    symbols.retain(|symbol| {
        symbol.kind != "Struct"
            || !declarations.contains(&(symbol.name.clone(), symbol.location.uri.clone()))
            || matches!(
                &symbol.location.source,
                SourceContext::Available(frame)
                    if rendered_struct_declaration(&frame.text, &symbol.name, &symbol.location.range)
            )
    });
}

#[test]
fn rendered_workspace_symbol_position_skips_docs_and_declaration_prefixes() {
    let range = crate::bridge::Range {
        start: crate::bridge::Position2D {
            line: 65,
            character: 1,
        },
        end: crate::bridge::Position2D {
            line: 65,
            character: 24,
        },
    };

    assert_eq!(
        rendered_workspace_symbol_position(
            "  64 | /// Adds two values.\n  65 | pub fn add(a: i32, b: i32) -> i32 {\n",
            "add",
            &range,
        ),
        Some((65, 8)),
    );
}

#[test]
fn rendered_struct_declaration_rejects_struct_uses() {
    let range = crate::bridge::Range {
        start: crate::bridge::Position2D {
            line: 2,
            character: 13,
        },
        end: crate::bridge::Position2D {
            line: 2,
            character: 18,
        },
    };
    assert!(!rendered_struct_declaration(
        "   1 | pub struct Point { x: f64 }\n   2 | let p = Point { x: 1.0 };\n",
        "Point",
        &range,
    ));
}

fn missing_call_hierarchy_item() -> crate::bridge::InspectSection<crate::bridge::InspectCalls> {
    crate::bridge::InspectSection::unavailable(
        "call hierarchy provider returned no item at the symbol selection",
    )
}

async fn inspect_if_requested<T>(
    requested: bool,
    request: impl Future<Output = Result<T, String>>,
) -> Option<Result<T, String>> {
    if !requested {
        return None;
    }
    Some(request.await)
}

impl SymbolHandleStore {
    fn new() -> Self {
        Self {
            entries: HashMap::new(),
            ttl: Duration::from_secs(15 * 60),
            max_entries: 1024,
        }
    }

    fn prune(&mut self) {
        let now = Instant::now();
        self.entries
            .retain(|_, target| now.duration_since(target.created_at) < self.ttl);
    }

    fn insert(&mut self, target: StoredSymbolTarget) -> SymbolHandle {
        self.prune();
        while self.entries.len() >= self.max_entries {
            let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, target)| target.created_at)
                .map(|(handle, _)| handle.clone())
            else {
                break;
            };
            self.entries.remove(&oldest);
        }
        let handle = SymbolHandle::new();
        self.entries.insert(handle.clone(), target);
        handle
    }

    fn resolve(&mut self, handle: &SymbolHandle) -> Result<StoredSymbolTarget, String> {
        self.prune();
        self.entries.get(handle).cloned().ok_or_else(|| {
            "invalid_symbol_handle: handle is missing, forged, expired, or belongs to another project; rerun symbol discovery"
                .to_owned()
        })
    }
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

fn call_hierarchy_snapshot_hash(items: &[CallHierarchyItemResult]) -> String {
    items
        .iter()
        .find_map(|item| match &item.source {
            Some(SourceContext::Available(frame)) => Some(frame.content_hash.clone()),
            Some(SourceContext::Deferred { resource }) => Some(resource.snapshot_hash.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

fn trim_workspace_symbol_batch(batch: &mut WorkspaceSymbolBatchResult) {
    while serde_json::to_vec(batch).map_or(usize::MAX, |encoded| encoded.len()) > batch.max_bytes {
        let Some(result) = batch
            .entries
            .iter_mut()
            .rev()
            .filter_map(|entry| entry.result.as_mut())
            .find(|result| !result.symbols.is_empty())
        else {
            batch.truncated = true;
            return;
        };
        result.symbols.pop();
        result.returned = result.symbols.len();
        result.remaining = result.total.saturating_sub(result.returned);
        result.truncated = true;
        batch.returned = batch.returned.saturating_sub(1);
        batch.truncated = true;
    }
}
#[derive(Debug, Serialize, Deserialize)]
struct WorkspaceSymbolPageState {
    total: usize,
    snapshot_identity: String,
    symbols: Vec<WorkspaceSymbol>,
}

#[derive(Debug, Serialize, Deserialize)]
struct DocumentSymbolPageState {
    total: usize,
    snapshot_identity: String,
    document_version: Option<i32>,
    project_relative_path: Option<String>,
    source_resource: DeferredResourceReference,
    filters: DocumentSymbolOptions,
    symbols: Vec<crate::bridge::Symbol>,
}

#[derive(Debug, Serialize, Deserialize)]
struct DiagnosticsPageState {
    file_path: String,
    fresh: bool,
    result: DiagnosticsResult,
}

fn remaining_diagnostic_occurrences(
    diagnostics: &VecDeque<crate::bridge::Diagnostic>,
    preserve_locations: bool,
) -> usize {
    diagnostics
        .iter()
        .map(|diagnostic| {
            if preserve_locations {
                diagnostic.context.occurrences.len()
            } else {
                diagnostic.context.occurrence_count
            }
        })
        .sum()
}

fn set_diagnostics_page_metadata(
    result: &mut DiagnosticsResult,
    remaining_diagnostics: usize,
    remaining_groups: usize,
    source_truncated: bool,
) {
    const CURSOR_PLACEHOLDER: &str = "mcpls-deferred:///00000000-0000-0000-0000-000000000000";

    result.returned_groups = result.diagnostics.len();
    result.remaining_groups = remaining_groups;
    result.omitted_groups = result.remaining_groups;
    result.remaining_diagnostics = remaining_diagnostics;
    let has_continuation = remaining_groups != 0;
    result.next_cursor = has_continuation.then(|| CURSOR_PLACEHOLDER.to_owned());
    result.truncated = source_truncated || has_continuation;
}

fn finish_diagnostics_page_metadata(
    result: &mut DiagnosticsResult,
    remaining: &VecDeque<crate::bridge::Diagnostic>,
    source_truncated: bool,
) {
    set_diagnostics_page_metadata(
        result,
        remaining_diagnostic_occurrences(remaining, result.filters.preserve_locations),
        remaining.len(),
        source_truncated,
    );
}

fn bounded_diagnostics_page(
    mut state: DiagnosticsPageState,
    max_items: usize,
    max_bytes: usize,
) -> Result<(DiagnosticsResult, Option<DiagnosticsPageState>), String> {
    let source_truncated = state.result.truncated;
    let preserve_locations = state.result.filters.preserve_locations;
    let mut remaining = VecDeque::from(std::mem::take(&mut state.result.diagnostics));
    let mut page = DiagnosticsResult {
        diagnostics: Vec::new(),
        source_resource: state.result.source_resource.clone(),
        total_diagnostics: state.result.total_diagnostics,
        returned_diagnostics: 0,
        remaining_diagnostics: state.result.total_diagnostics,
        total_groups: state.result.total_groups,
        returned_groups: 0,
        omitted_groups: state.result.total_groups,
        remaining_groups: state.result.total_groups,
        next_cursor: None,
        snapshot_identity: state.result.snapshot_identity.clone(),
        max_bytes: Some(max_bytes),
        truncated: source_truncated,
        filters: state.result.filters.clone(),
        cache: state.result.cache.clone(),
    };

    while page.diagnostics.len() < max_items {
        let Some(mut diagnostic) = remaining.pop_front() else {
            break;
        };

        if !preserve_locations {
            let represented = diagnostic.context.occurrence_count;
            let mut candidate = page.clone();
            candidate.diagnostics.push(diagnostic.clone());
            candidate.returned_diagnostics += represented;
            set_diagnostics_page_metadata(
                &mut candidate,
                remaining_diagnostic_occurrences(&remaining, false),
                remaining.len(),
                source_truncated,
            );
            if serde_json::to_vec(&candidate).map_or(usize::MAX, |encoded| encoded.len())
                > max_bytes
            {
                remaining.push_front(diagnostic);
                if page.diagnostics.is_empty() {
                    return Err(
                        "byte_limit is too small to return one diagnostic group identity"
                            .to_owned(),
                    );
                }
                break;
            }
            page = candidate;
            continue;
        }

        let offset = diagnostic.context.occurrence_offset;
        let mut occurrences = VecDeque::from(std::mem::take(&mut diagnostic.context.occurrences));
        let mut page_group = diagnostic.clone();
        page_group.context.occurrences.clear();
        let page_before_group = page.clone();
        let mut accepted = 0;

        while let Some(occurrence) = occurrences.pop_front() {
            let mut candidate_group = page_group.clone();
            candidate_group.context.occurrences.push(occurrence.clone());
            let mut candidate = page_before_group.clone();
            candidate.diagnostics.push(candidate_group.clone());
            candidate.returned_diagnostics =
                page_before_group.returned_diagnostics + candidate_group.context.occurrences.len();

            let remaining_occurrences =
                occurrences.len() + remaining_diagnostic_occurrences(&remaining, true);
            let remaining_groups = remaining.len() + usize::from(!occurrences.is_empty());
            set_diagnostics_page_metadata(
                &mut candidate,
                remaining_occurrences,
                remaining_groups,
                source_truncated,
            );
            if serde_json::to_vec(&candidate).map_or(usize::MAX, |encoded| encoded.len())
                > max_bytes
            {
                occurrences.push_front(occurrence);
                break;
            }

            accepted += 1;
            page_group = candidate_group;
            page = candidate;
        }

        if accepted == 0 {
            diagnostic.context.occurrences = occurrences.into();
            remaining.push_front(diagnostic);
            if page.diagnostics.is_empty() {
                return Err(
                    "byte_limit is too small to return one diagnostic occurrence identity"
                        .to_owned(),
                );
            }
            break;
        }

        if !occurrences.is_empty() {
            diagnostic.context.occurrence_offset = offset + accepted;
            diagnostic.context.occurrences = occurrences.into();
            remaining.push_front(diagnostic);
            break;
        }
    }

    finish_diagnostics_page_metadata(&mut page, &remaining, source_truncated);
    let continuation = (!remaining.is_empty()).then(|| {
        state.result.diagnostics = remaining.into();
        state
    });
    Ok((page, continuation))
}

fn flatten_document_symbols(symbols: Vec<crate::bridge::Symbol>) -> Vec<crate::bridge::Symbol> {
    fn flatten(symbol: crate::bridge::Symbol, output: &mut Vec<crate::bridge::Symbol>) {
        let mut symbol = symbol;
        let children = symbol.children.take().unwrap_or_default();
        output.push(symbol);
        children
            .into_iter()
            .for_each(|child| flatten(child, output));
    }

    let mut output = Vec::new();
    symbols
        .into_iter()
        .for_each(|symbol| flatten(symbol, &mut output));
    output
}

fn clear_document_symbol_sources(symbols: &mut [crate::bridge::Symbol]) {
    for symbol in symbols {
        symbol.source = None;
        if let Some(children) = &mut symbol.children {
            clear_document_symbol_sources(children);
        }
    }
}

fn document_symbol_matches(symbol: &crate::bridge::Symbol, has_query: bool) -> bool {
    !has_query || symbol.match_class.is_some()
}

fn bounded_document_symbol_page(
    state: DocumentSymbolPageState,
    max_items: usize,
    max_bytes: usize,
) -> Result<(DocumentSymbolsResult, Option<DocumentSymbolPageState>), String> {
    const CURSOR_PLACEHOLDER: &str = "mcpls-deferred:///00000000-0000-0000-0000-000000000000";

    let DocumentSymbolPageState {
        total,
        snapshot_identity,
        document_version,
        project_relative_path,
        source_resource,
        filters,
        symbols,
    } = state;
    let has_query = filters.query.is_some();
    let mut remaining = VecDeque::from(symbols);
    let mut result = DocumentSymbolsResult {
        symbols: Vec::new(),
        project_relative_path: project_relative_path.clone(),
        source_resource: Some(source_resource.clone()),
        total,
        returned: 0,
        remaining: total,
        next_cursor: None,
        snapshot_identity: Some(snapshot_identity.clone()),
        document_version,
        max_bytes: Some(max_bytes),
        truncated: false,
        filters: filters.clone(),
    };

    while let Some(symbol) = remaining.pop_front() {
        let matched = document_symbol_matches(&symbol, has_query);
        if matched && result.returned >= max_items {
            remaining.push_front(symbol);
            break;
        }
        result.symbols.push(symbol);
        result.returned += usize::from(matched);
        result.remaining = remaining
            .iter()
            .filter(|symbol| document_symbol_matches(symbol, has_query))
            .count();
        result.truncated = !remaining.is_empty();
        result.next_cursor = result.truncated.then(|| CURSOR_PLACEHOLDER.to_owned());
        if serde_json::to_vec(&result).map_or(usize::MAX, |encoded| encoded.len()) <= max_bytes {
            continue;
        }

        let Some(symbol) = result.symbols.pop() else {
            return Err("document-symbol page lost its inserted symbol".to_owned());
        };
        result.returned -= usize::from(matched);
        remaining.push_front(symbol);
        if result.symbols.is_empty() {
            return Err("max_bytes is too small to return one document symbol identity".to_owned());
        }
        break;
    }

    result.remaining = remaining
        .iter()
        .filter(|symbol| document_symbol_matches(symbol, has_query))
        .count();
    result.truncated = !remaining.is_empty();
    result.next_cursor = result.truncated.then(|| CURSOR_PLACEHOLDER.to_owned());
    let remaining = (!remaining.is_empty()).then(|| DocumentSymbolPageState {
        total,
        snapshot_identity,
        document_version,
        project_relative_path,
        source_resource,
        filters,
        symbols: remaining.into(),
    });
    Ok((result, remaining))
}

fn workspace_symbol_snapshot_identity(symbols: &[WorkspaceSymbol]) -> Result<String, String> {
    let encoded = serde_json::to_vec(symbols)
        .map_err(|error| format!("failed to identify workspace-symbol snapshot: {error}"))?;
    Ok(format!("{:x}", Sha256::digest(encoded)))
}

fn bounded_workspace_symbol_page(
    state: WorkspaceSymbolPageState,
    max_items: usize,
    max_bytes: usize,
) -> Result<(WorkspaceSymbolResult, Option<WorkspaceSymbolPageState>), String> {
    const CURSOR_PLACEHOLDER: &str = "mcpls-deferred:///00000000-0000-0000-0000-000000000000";

    let total = state.total;
    let snapshot_identity = state.snapshot_identity;
    let mut symbols = state.symbols;
    let mut remaining = VecDeque::from(symbols.split_off(max_items.min(symbols.len())));
    let mut result = WorkspaceSymbolResult {
        symbols,
        total,
        returned: 0,
        remaining: 0,
        next_cursor: None,
        snapshot_identity: Some(snapshot_identity.clone()),
        max_bytes: Some(max_bytes),
        truncated: false,
    };

    loop {
        result.returned = result.symbols.len();
        result.remaining = remaining.len();
        result.truncated = !remaining.is_empty();
        result.next_cursor = result.truncated.then(|| CURSOR_PLACEHOLDER.to_owned());
        if serde_json::to_vec(&result).map_or(usize::MAX, |encoded| encoded.len()) <= max_bytes {
            break;
        }
        let Some(symbol) = result.symbols.pop() else {
            return Err(
                "max_bytes is too small to return one workspace symbol identity".to_owned(),
            );
        };
        remaining.push_front(symbol);
    }

    let remaining = (!remaining.is_empty()).then(|| WorkspaceSymbolPageState {
        total,
        snapshot_identity,
        symbols: remaining.into(),
    });
    Ok((result, remaining))
}

impl ProjectRuntime {
    #[cfg(test)]
    fn new(translator: Translator) -> Self {
        Self::with_edit_safety(translator, None)
    }

    #[cfg(test)]
    fn with_edit_safety(translator: Translator, edit_safety: Option<EditSafetyConfig>) -> Self {
        Self::with_deferred_results_scoped(
            translator,
            edit_safety,
            std::sync::Arc::new(std::sync::Mutex::new(DeferredResultStore::new())),
            None,
        )
    }

    fn with_deferred_results_scoped(
        translator: Translator,
        edit_safety: Option<EditSafetyConfig>,
        deferred_results: std::sync::Arc<std::sync::Mutex<DeferredResultStore>>,
        deferred_scope: Option<String>,
    ) -> Self {
        Self {
            translator,
            edit_plans: EditPlanStore::for_project(),
            edit_safety,
            code_actions: CodeActionStore::new(),
            symbol_handles: std::sync::Mutex::new(SymbolHandleStore::new()),
            workspace_symbol_results: std::sync::Mutex::new(HashMap::new()),
            inspect_symbol_batch_pages: std::sync::Mutex::new(InspectSymbolBatchPageStore::new()),
            deferred_results,
            deferred_scope,
            inline_module_checks: HashMap::new(),
            applied_edit_receipts: VecDeque::new(),
            applied_edit_receipt_bytes: 0,
            edit_conflicts: VecDeque::new(),
            active_edit_workers: 0,
            activation_health: ActivationHealth::Ready,
            generation: 0,
            automatic_restart: AutomaticRestartPolicy::default(),
        }
    }

    const fn record_activation(&mut self, health: ActivationHealth) {
        self.activation_health = health;
    }

    fn readiness_status(&self) -> ProjectStatus {
        activation_status(self.activation_health, self.translator.is_initializing())
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

    fn source_handle(
        &self,
        source: &SourceContext,
        line: u32,
        character: u32,
    ) -> Option<SymbolHandle> {
        let SourceContext::Available(frame) = source else {
            return None;
        };
        let snapshot = frame.document_version.map_or_else(
            || SourceSnapshot::Hash(frame.content_hash.clone()),
            SourceSnapshot::Version,
        );
        Some(
            self.symbol_handles
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(StoredSymbolTarget::new(
                    PathBuf::from(&frame.path),
                    line,
                    character,
                    snapshot,
                )),
        )
    }

    fn attach_location_handle(&self, location: &mut crate::bridge::Location) {
        location.symbol_handle = self.source_handle(
            &location.source,
            location.range.start.line,
            location.range.start.character,
        );
    }

    fn attach_location_handles<'a>(
        &self,
        locations: impl IntoIterator<Item = &'a mut crate::bridge::Location>,
    ) {
        locations
            .into_iter()
            .for_each(|location| self.attach_location_handle(location));
    }

    async fn attach_workspace_symbol_handle(
        &self,
        symbol: &mut WorkspaceSymbol,
        snapshots: &mut HashMap<PathBuf, (Option<i32>, String, String)>,
    ) {
        let location = &mut symbol.location;
        let Some(path) = location.path.as_deref().map(PathBuf::from) else {
            return;
        };
        let (line, character, snapshot) = match &location.source {
            SourceContext::Available(frame) => {
                let position =
                    rendered_workspace_symbol_position(&frame.text, &symbol.name, &location.range)
                        .unwrap_or((location.range.start.line, location.range.start.character));
                let snapshot = frame.document_version.map_or_else(
                    || SourceSnapshot::Hash(frame.content_hash.clone()),
                    SourceSnapshot::Version,
                );
                (position.0, position.1, snapshot)
            }
            SourceContext::Deferred { resource } => {
                let snapshot = resource.document_version.map_or_else(
                    || SourceSnapshot::Hash(resource.snapshot_hash.clone()),
                    SourceSnapshot::Version,
                );
                let position = self
                    .workspace_symbol_position(&path, &symbol.name, &location.range, snapshots)
                    .await
                    .unwrap_or((location.range.start.line, location.range.start.character));
                (position.0, position.1, snapshot)
            }
            SourceContext::Unavailable {
                reason: SourceUnavailableReason::ResponseBudgetExhausted,
            } => {
                let Some((document_version, content_hash, source)) =
                    self.workspace_symbol_snapshot(&path, snapshots).await
                else {
                    return;
                };
                let position = source_symbol_position(&source, &symbol.name, &location.range)
                    .unwrap_or((location.range.start.line, location.range.start.character));
                let snapshot = document_version.map_or_else(
                    || SourceSnapshot::Hash(content_hash.to_owned()),
                    SourceSnapshot::Version,
                );
                (position.0, position.1, snapshot)
            }
            SourceContext::Unavailable { .. } => return,
        };
        location.symbol_handle = Some(
            self.symbol_handles
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(StoredSymbolTarget::new(path, line, character, snapshot)),
        );
    }

    async fn attach_workspace_symbol_handles<'a>(
        &self,
        symbols: impl IntoIterator<Item = &'a mut WorkspaceSymbol>,
    ) {
        let mut snapshots = HashMap::new();
        for symbol in symbols {
            self.attach_workspace_symbol_handle(symbol, &mut snapshots)
                .await;
        }
    }

    async fn workspace_symbol_position(
        &self,
        path: &Path,
        name: &str,
        range: &crate::bridge::Range,
        snapshots: &mut HashMap<PathBuf, (Option<i32>, String, String)>,
    ) -> Option<(u32, u32)> {
        let (_, _, source) = self.workspace_symbol_snapshot(path, snapshots).await?;
        source_symbol_position(source, name, range)
    }

    async fn workspace_symbol_snapshot<'a>(
        &self,
        path: &Path,
        snapshots: &'a mut HashMap<PathBuf, (Option<i32>, String, String)>,
    ) -> Option<&'a (Option<i32>, String, String)> {
        if !snapshots.contains_key(path) {
            let (_, version, hash, source) = self.translator.source_snapshot(path).await.ok()?;
            snapshots.insert(path.to_path_buf(), (version, hash, source));
        }
        snapshots.get(path)
    }

    fn attach_reference_handle(&self, reference: &mut crate::bridge::ReferenceUse) {
        reference.symbol_handle = reference.snapshot.as_ref().map(|snapshot| {
            let source_snapshot = snapshot.document_version.map_or_else(
                || SourceSnapshot::Hash(snapshot.content_hash.clone()),
                SourceSnapshot::Version,
            );
            self.symbol_handles
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(StoredSymbolTarget::new(
                    PathBuf::from(&snapshot.path),
                    reference.range[0],
                    reference.range[1],
                    source_snapshot,
                ))
        });
    }

    async fn resolve_symbol_target(
        &self,
        handle: &SymbolHandle,
    ) -> Result<StoredSymbolTarget, String> {
        let target = self
            .symbol_handles
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .resolve(handle)?;
        let (_, version, hash, _) = self
            .translator
            .source_snapshot(&target.file_path)
            .await
            .map_err(|error| {
                format!(
                    "stale_symbol_handle: source is unavailable; rerun symbol discovery: {error}"
                )
            })?;
        let current = match &target.snapshot {
            SourceSnapshot::Version(expected) => version == Some(*expected),
            SourceSnapshot::Hash(expected) => hash == *expected,
        };
        current.then_some(target).ok_or_else(|| {
            "stale_symbol_handle: source changed; rerun symbol discovery to refresh the handle"
                .to_owned()
        })
    }

    fn has_active_workspace_roots(&self, roots: &[PathBuf]) -> bool {
        self.translator.has_active_workspace_roots(roots)
    }

    fn activation_is_reusable(&self, status: ProjectStatus, roots: &[PathBuf]) -> bool {
        match status {
            ProjectStatus::Starting | ProjectStatus::Ready => {
                self.has_active_workspace_roots(roots)
            }
            ProjectStatus::Degraded => self.translator.has_workspace_roots(roots),
            ProjectStatus::Restarting
            | ProjectStatus::Dormant
            | ProjectStatus::Stopping
            | ProjectStatus::Stopped
            | ProjectStatus::Failed => false,
        }
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

    async fn preview_edit(
        &mut self,
        project_id: &str,
        edit: WorkspaceEdit,
        encoding: PositionEncoding,
        root: &Path,
    ) -> Result<PreviewArtifact, String> {
        let boundary = WorkspaceBoundary::new(root).map_err(|error| error.to_string())?;
        let limits = PreviewLimits::default();
        let documents =
            refresh_workspace_edit_documents(&edit, self.translator.document_tracker(), limits)
                .await
                .map_err(|error| error.to_string())?;
        let artifact =
            preview_workspace_edit(&boundary, project_id, edit, encoding, &documents, limits)
                .map_err(|error| error.to_string())?;
        self.edit_plans
            .insert(artifact.plan.clone())
            .map_err(|error| error.to_string())?;
        Ok(artifact)
    }

    async fn preview_generated_edit(
        &mut self,
        project_id: &str,
        request: GeneratedEditRequest,
        encoding: PositionEncoding,
        root: &Path,
    ) -> Result<GeneratedEditPreview, String> {
        let (supported, edit, create_empty_plan) = match request {
            GeneratedEditRequest::Rename {
                file_path,
                line,
                character,
                new_name,
            } => (
                true,
                self.rename_workspace_edit(file_path, line, character, new_name)
                    .await?,
                true,
            ),
            GeneratedEditRequest::Format {
                file_path,
                tab_size,
                insert_spaces,
            } => (
                true,
                self.format_workspace_edit(file_path, tab_size, insert_spaces)
                    .await?,
                true,
            ),
            GeneratedEditRequest::RangeFormat {
                file_path,
                start,
                end,
                tab_size,
                insert_spaces,
            } => {
                let result = self
                    .translator
                    .request_range_format_workspace_edit(
                        file_path,
                        start,
                        end,
                        tab_size,
                        insert_spaces,
                    )
                    .await
                    .map_err(|error| error.to_string())?;
                (result.supported, result.edit, false)
            }
            GeneratedEditRequest::MoveItem {
                file_path,
                start,
                end,
                direction,
            } => {
                let result = self
                    .translator
                    .request_move_item_workspace_edit(file_path, start, end, &direction)
                    .await
                    .map_err(|error| error.to_string())?;
                (result.supported, result.edit, false)
            }
        };
        let artifact = match edit.or_else(|| create_empty_plan.then(WorkspaceEdit::default)) {
            Some(edit) => Some(self.preview_edit(project_id, edit, encoding, root).await?),
            None => None,
        };
        Ok(GeneratedEditPreview {
            supported,
            artifact,
        })
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
        let (edit, matches, snapshots, verification, producer) = match dialect {
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
                    Vec::new(),
                    VerificationStatus::SemanticVerified,
                    EditProducer::RustAnalyzer,
                )
            }
            StructuralDialect::AstGrep => {
                let language = language_id.ok_or_else(|| {
                    "ast_grep requires an explicit language_id; syntax is never inferred or translated"
                        .to_string()
                })?;
                let StructuralSearchResult {
                    edit,
                    matches,
                    snapshots,
                } = self
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
                    snapshots,
                    VerificationStatus::StructuralUnverified,
                    EditProducer::StructuralAstGrep,
                )
            }
        };
        let scanned_files = self.validate_structural_snapshots(&snapshots).await?;
        let artifact = match edit {
            Some(edit) => {
                let mut artifact = self.preview_edit(project_id, edit, encoding, root).await?;
                artifact.verification = Some(verification);
                artifact.producer = Some(producer);
                Some(artifact)
            }
            None => None,
        };
        let matched_files = artifact.as_ref().map_or(scanned_files, |artifact| {
            let matched_paths = matches
                .iter()
                .map(|matched| matched.path.as_path())
                .collect::<HashSet<_>>();
            artifact
                .plan
                .files()
                .iter()
                .filter(|file| matched_paths.contains(file.path().as_path()))
                .map(|file| StructuralMatchedFile {
                    path: file.path().clone(),
                    content_hash: file.content_hash().to_owned(),
                    document_version: file.version(),
                    total_bytes: file.original_content().len(),
                    total_lines: u32::try_from(file.original_content().lines().count().max(1))
                        .unwrap_or(u32::MAX),
                })
                .collect()
        });
        Ok(StructuralPreview {
            artifact,
            dialect,
            matches,
            matched_files,
            parse_only,
        })
    }

    async fn validate_structural_snapshots(
        &self,
        snapshots: &[StructuralFileSnapshot],
    ) -> Result<Vec<StructuralMatchedFile>, String> {
        let mut matched_files = Vec::with_capacity(snapshots.len());
        for expected in snapshots {
            let (path, document_version, content_hash, content) = self
                .translator
                .source_snapshot(&expected.path)
                .await
                .map_err(|error| error.to_string())?;
            if content_hash != expected.content_hash {
                return Err(format!(
                    "stale_resource: {} changed during structural search; rerun the preview",
                    path.display()
                ));
            }
            matched_files.push(StructuralMatchedFile {
                path,
                content_hash,
                document_version,
                total_bytes: content.len(),
                total_lines: u32::try_from(content.lines().count().max(1)).unwrap_or(u32::MAX),
            });
        }
        Ok(matched_files)
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
        let mut artifact = self
            .preview_edit(project_id, edit, request.encoding, root)
            .await?;
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
            .handle_document_symbols(
                source_path.display().to_string(),
                DocumentSymbolOptions::internal_tree(),
            )
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
            .reconciled_snapshot(&source_path)
            .await
            .map_err(|error| error.to_string())?
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
        let mut artifact = self.preview_edit(project_id, edit, encoding, root).await?;
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

    fn inspect_edit_plan(
        &self,
        plan_id: &PlanId,
        project_id: &str,
    ) -> Result<crate::edit_plan::EditPlanApprovalSummary, String> {
        self.edit_plans
            .get_for_project(plan_id, project_id)
            .map(EditPlan::approval_summary)
            .map_err(|error| error.to_string())
    }

    fn read_edit_plan_diff(&self, plan_id: &PlanId, project_id: &str) -> Result<String, String> {
        self.edit_plans
            .get_for_project(plan_id, project_id)
            .map(EditPlan::complete_unified_diff)
            .map_err(|error| error.to_string())
    }

    fn read_applied_edit_detail(
        &self,
        plan_id: &PlanId,
        project_id: &str,
    ) -> Result<String, String> {
        self.applied_edit_receipts
            .iter()
            .find(|receipt| &receipt.plan_id == plan_id)
            .map(|receipt| serde_json::to_string(&receipt.detail_json()))
            .transpose()
            .map_err(|serialization| serialization.to_string())?
            .ok_or_else(|| {
                format!("applied edit result for project {project_id} is no longer retained")
            })
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

    fn remember_edit_conflict(&mut self, conflict: EditConflict) -> EditConflict {
        self.edit_conflicts
            .retain(|existing| existing.plan_id != conflict.plan_id);
        if self.edit_conflicts.len() >= MAX_APPLIED_EDIT_RECEIPTS {
            self.edit_conflicts.pop_front();
        }
        self.edit_conflicts.push_back(conflict.clone());
        conflict
    }

    /// Apply a plan directly for the runtime-only test and embedding path.
    ///
    /// The project actor uses the same preparation and commit functions, but
    /// runs the filesystem phase on its bounded blocking worker. Keeping this
    /// small adapter preserves the runtime API without reintroducing the
    /// actor-wide mutation lock into production requests.
    #[cfg(test)]
    async fn apply_edit_plan_with_context(
        &mut self,
        plan_id: &PlanId,
        project_id: &str,
        root: &Path,
        session_id: Option<String>,
        principal: Option<String>,
    ) -> Result<AppliedEditPlan, String> {
        let lease = EditCoordinator::new()
            .try_acquire(plan_id.as_str(), Vec::new())
            .map_err(|contention| format!("edit plan is busy: {contention:?}"))?;
        let prepared = self.prepare_edit_plan_with_context(
            plan_id, project_id, root, session_id, principal, lease,
        )?;
        let prepared = match prepared {
            PreparedEditResult::AlreadyApplied(applied) => return Ok(applied),
            PreparedEditResult::AlreadyConflicted(conflict) => return Err(conflict.reason),
            PreparedEditResult::Ready(prepared) => *prepared,
        };
        let apply_result = match prepared.backup_policy.as_ref() {
            Some(policy) => apply_plan_with_documents_and_backup(
                &prepared.boundary,
                &prepared.plan,
                &prepared.documents,
                policy,
            ),
            None => {
                apply_plan_with_documents(&prepared.boundary, &prepared.plan, &prepared.documents)
            }
        };
        match self.finish_prepared_edit(prepared, apply_result).await? {
            ApplyEditPlanOutcome::Applied(applied) => Ok(applied),
            ApplyEditPlanOutcome::Conflict(conflict) => Err(conflict.reason),
            ApplyEditPlanOutcome::NotReady(_) => Err("edit plan was not ready".to_owned()),
        }
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
            .handle_document_symbols(
                check.source_path.display().to_string(),
                DocumentSymbolOptions::internal_tree(),
            )
            .await;
        let destination_symbols = self
            .translator
            .handle_document_symbols(
                check.destination_path.display().to_string(),
                DocumentSymbolOptions::internal_tree(),
            )
            .await;
        let source_diagnostics = self
            .translator
            .handle_actor_diagnostics(
                check.source_path.display().to_string(),
                DiagnosticOptions::default(),
            )
            .await;
        let destination_diagnostics = self
            .translator
            .handle_actor_diagnostics(
                check.destination_path.display().to_string(),
                DiagnosticOptions::default(),
            )
            .await;
        let references = self
            .translator
            .handle_references(
                check.source_path.display().to_string(),
                check.source_position.line.saturating_add(1),
                check.source_position.character.saturating_add(1),
                true,
                SemanticResultLimits::default(),
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

    fn prepare_edit_plan_with_context(
        &mut self,
        plan_id: &PlanId,
        project_id: &str,
        root: &Path,
        session_id: Option<String>,
        principal: Option<String>,
        lease: EditLease,
    ) -> Result<PreparedEditResult, String> {
        if let Some(applied) = self
            .applied_edit_receipts
            .iter()
            .find(|applied| &applied.plan_id == plan_id)
        {
            return Ok(PreparedEditResult::AlreadyApplied(applied.clone()));
        }
        if let Some(conflict) = self
            .edit_conflicts
            .iter()
            .find(|conflict| &conflict.plan_id == plan_id)
        {
            return Ok(PreparedEditResult::AlreadyConflicted(conflict.clone()));
        }
        let workspace_root = self
            .edit_plans
            .get_for_project(plan_id, project_id)
            .map_err(|error| error.to_string())?
            .workspace_root()
            .map_or_else(|| root.to_path_buf(), Path::to_path_buf);
        let boundary = WorkspaceBoundary::new(workspace_root).map_err(|error| error.to_string())?;
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
        Ok(PreparedEditResult::Ready(Box::new(PreparedEditPlan {
            plan,
            boundary,
            backup_policy,
            semantic_check,
            resource_operations,
            text_changes,
            open_documents,
            audit,
            documents: std::sync::Arc::clone(self.translator.document_tracker()),
            lease,
        })))
    }

    async fn finish_prepared_edit(
        &mut self,
        prepared: PreparedEditPlan,
        apply_result: Result<ApplyReport, ApplyError>,
    ) -> Result<ApplyEditPlanOutcome, String> {
        let PreparedEditPlan {
            plan,
            boundary: _boundary,
            backup_policy: _backup_policy,
            semantic_check,
            resource_operations,
            text_changes,
            open_documents,
            audit,
            documents: _documents,
            lease,
        } = prepared;
        let ApplyReport { committed_files } = match apply_result {
            Ok(report) => report,
            Err(ApplyError::Stale(_)) => {
                let changed_paths = plan
                    .files()
                    .iter()
                    .map(|snapshot| snapshot.path().clone())
                    .collect();
                let conflict = self.remember_edit_conflict(EditConflict {
                    plan_id: plan.id().clone(),
                    changed_paths,
                    reason: "snapshot_changed".to_owned(),
                });
                drop(lease);
                return Ok(ApplyEditPlanOutcome::Conflict(conflict));
            }
            Err(ApplyError::TopologyChanged { .. }) => {
                let changed_paths = plan
                    .files()
                    .iter()
                    .map(|snapshot| snapshot.path().clone())
                    .collect();
                let conflict = self.remember_edit_conflict(EditConflict {
                    plan_id: plan.id().clone(),
                    changed_paths,
                    reason: "snapshot_changed".to_owned(),
                });
                drop(lease);
                return Ok(ApplyEditPlanOutcome::Conflict(conflict));
            }
            Err(ApplyError::Operation(OperationValidationError::DestinationExists(path))) => {
                let conflict = self.remember_edit_conflict(EditConflict {
                    plan_id: plan.id().clone(),
                    changed_paths: vec![path],
                    reason: "snapshot_changed".to_owned(),
                });
                drop(lease);
                return Ok(ApplyEditPlanOutcome::Conflict(conflict));
            }
            Err(error) => {
                return Err(self.record_edit_failure(audit, error.to_string()));
            }
        };
        let audit_failure = self
            .edit_plans
            .record_audit_with_policy(audit.clone().committed(committed_files.clone()))
            .err()
            .map(|error| error.to_string());
        if audit_failure.is_some() {
            self.edit_plans
                .record_audit(audit.committed(committed_files.clone()));
        }
        // Persist a receipt and release the path lease at the filesystem
        // commit point. Provider/LSP convergence is best-effort and must not
        // hold another editor's paths hostage.
        let mut applied = AppliedEditPlan {
            plan_id: plan.id().clone(),
            operations: plan.operations().to_vec(),
            unified_diff: plan.unified_diff().to_string(),
            complete_unified_diff: plan.complete_unified_diff(),
            committed_files,
            verification: None,
            provider_synchronization: Vec::new(),
        };
        let applied_bytes = applied.estimated_bytes();
        while self.applied_edit_receipts.len() >= MAX_APPLIED_EDIT_RECEIPTS
            || self
                .applied_edit_receipt_bytes
                .saturating_add(applied_bytes)
                > MAX_APPLIED_EDIT_RECEIPT_BYTES
        {
            let Some(evicted) = self.applied_edit_receipts.pop_front() else {
                break;
            };
            self.applied_edit_receipt_bytes = self
                .applied_edit_receipt_bytes
                .saturating_sub(evicted.estimated_bytes());
        }
        if applied_bytes <= MAX_APPLIED_EDIT_RECEIPT_BYTES {
            self.applied_edit_receipt_bytes = self
                .applied_edit_receipt_bytes
                .saturating_add(applied_bytes);
            self.applied_edit_receipts.push_back(applied.clone());
        }
        drop(lease);

        let (provider_synchronization, verification) = self
            .synchronize_applied_edit(
                &resource_operations,
                &text_changes,
                open_documents,
                semantic_check.as_ref(),
                audit_failure,
            )
            .await;
        applied.verification = verification;
        applied.provider_synchronization = provider_synchronization;
        if let Some(receipt) = self
            .applied_edit_receipts
            .iter_mut()
            .find(|receipt| receipt.plan_id == applied.plan_id)
        {
            self.applied_edit_receipt_bytes = self
                .applied_edit_receipt_bytes
                .saturating_sub(receipt.estimated_bytes());
            *receipt = applied.clone();
            self.applied_edit_receipt_bytes = self
                .applied_edit_receipt_bytes
                .saturating_add(receipt.estimated_bytes());
        }
        Ok(ApplyEditPlanOutcome::Applied(applied))
    }

    async fn synchronize_applied_edit(
        &mut self,
        resource_operations: &[FileOperation],
        text_changes: &[(PathBuf, String)],
        open_documents: Vec<(PathBuf, i32, String)>,
        semantic_check: Option<&InlineModuleSemanticCheck>,
        audit_failure: Option<String>,
    ) -> (Vec<ProviderSynchronization>, Option<VerificationStatus>) {
        let mut document_sync_failures = Vec::new();
        let mut tracker_sync_failures = Vec::new();
        for (path, version, content) in open_documents {
            match self
                .translator
                .apply_open_document_content(&path, version, content)
                .await
            {
                Ok(failures) => document_sync_failures.extend(failures),
                Err(error) => tracker_sync_failures.push(error.to_string()),
            }
        }
        let mut provider_synchronization = self
            .translator
            .synchronize_resource_operations(resource_operations)
            .await;
        for result in self.translator.synchronize_text_changes(text_changes).await {
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
        for error in tracker_sync_failures {
            merge_provider_synchronization(
                &mut provider_synchronization,
                ProviderSynchronization {
                    provider: "document_tracker".to_string(),
                    synchronized: false,
                    watched_file_notifications: 0,
                    message: Some(error),
                },
            );
        }
        let verification = if let Some(check) = semantic_check {
            Some(self.verify_inline_module_after_apply(check).await)
        } else {
            None
        };
        if let Some(error) = audit_failure {
            provider_synchronization.push(ProviderSynchronization {
                provider: "audit".to_string(),
                synchronized: false,
                watched_file_notifications: 0,
                message: Some(error),
            });
        }
        (provider_synchronization, verification)
    }

    async fn hover(
        &self,
        file_path: String,
        line: u32,
        character: u32,
    ) -> Result<HoverResult, String> {
        let mut result = self
            .translator
            .handle_hover(file_path.clone(), line, character)
            .await
            .map_err(|error| error.to_string())?;
        let target = result.range.as_ref().map_or((line, character), |range| {
            (range.start.line, range.start.character)
        });
        result.symbol_handle = self.source_handle(&result.source, target.0, target.1);
        Ok(result)
    }

    async fn definition(
        &self,
        file_path: String,
        line: u32,
        character: u32,
    ) -> Result<DefinitionResult, String> {
        let mut result = self
            .translator
            .handle_definition(file_path, line, character)
            .await
            .map_err(|error| error.to_string())?;
        self.attach_location_handles(&mut result.locations);
        Ok(result)
    }

    async fn references(
        &self,
        file_path: String,
        line: u32,
        character: u32,
        include_declaration: bool,
        limits: SemanticResultLimits,
        page_offset: Option<usize>,
    ) -> Result<ReferencesResult, String> {
        let mut result = self
            .translator
            .handle_references_page(
                file_path,
                line,
                character,
                include_declaration,
                limits,
                page_offset,
            )
            .await
            .map_err(|error| error.to_string())?;
        for group in &mut result.groups {
            for reference in &mut group.references {
                self.attach_reference_handle(reference);
            }
        }
        if let Some(declaration) = result.declaration.as_mut() {
            self.attach_location_handle(declaration);
        }
        Ok(result)
    }

    async fn read_source_resource(
        &self,
        resource: SourceResource,
        max_response_bytes: usize,
    ) -> Result<SourceFrame, String> {
        self.translator
            .read_source_resource_with_max_bytes(&resource, max_response_bytes)
            .await
            .map_err(|error| error.to_string())
    }

    fn defer_inspect_section<T: Serialize>(
        &self,
        section: &mut crate::bridge::InspectSection<T>,
        snapshot_hash: &str,
    ) {
        let Some(data) = section.data.take() else {
            return;
        };
        let Ok(value) = serde_json::to_value(data) else {
            return;
        };
        let provider = section
            .provider
            .clone()
            .unwrap_or_else(|| "mcpls".to_owned());
        let reference = self
            .deferred_results
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert_scoped(
                value,
                snapshot_hash.to_owned(),
                self.deferred_scope.as_deref().unwrap_or_default(),
            );
        *section = crate::bridge::InspectSection::deferred(
            provider,
            section.total,
            section.returned,
            "response_budget_exhausted",
            reference,
        );
    }

    async fn resolve_symbol_handle(
        &self,
        symbol_handle: SymbolHandle,
    ) -> Result<ResolvedSymbolTarget, String> {
        let target = self.resolve_symbol_target(&symbol_handle).await?;
        Ok(ResolvedSymbolTarget {
            file_path: target.file_path.to_string_lossy().into_owned(),
            line: target.line,
            character: target.character,
        })
    }

    async fn diagnostics(
        &mut self,
        file_path: String,
        options: DiagnosticOptions,
    ) -> Result<DiagnosticsResult, String> {
        self.diagnostics_page(file_path, options, true).await
    }

    async fn diagnostics_page(
        &mut self,
        file_path: String,
        mut options: DiagnosticOptions,
        fresh: bool,
    ) -> Result<DiagnosticsResult, String> {
        let page_token = options.page_token.take();
        let scope = self.deferred_scope.clone().unwrap_or_default();
        let state = if let Some(page_token) = page_token {
            let token = page_token
                .strip_prefix("mcpls-deferred:///")
                .ok_or_else(|| {
                    "page_token must be the next_cursor returned by get_diagnostics".to_owned()
                })?;
            let value = self
                .deferred_results
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .read_scoped(token, &scope)?;
            let state: DiagnosticsPageState = serde_json::from_value(value)
                .map_err(|error| format!("invalid diagnostics page: {error}"))?;
            if state.file_path != file_path || state.fresh != fresh {
                return Err("page_token belongs to a different diagnostics request".to_owned());
            }
            state
        } else {
            if options.item_limit == 0 || options.item_limit > 1_000 {
                return Err("item_limit must be between 1 and 1000".to_owned());
            }
            if !(4_096..=1_048_576).contains(&options.byte_limit) {
                return Err("byte_limit must be between 4096 and 1048576".to_owned());
            }

            let mut collection_options = options.clone();
            collection_options.item_limit = usize::MAX;
            let mut result = if fresh {
                self.translator
                    .handle_actor_diagnostics(file_path.clone(), collection_options)
                    .await
                    .map_err(|error| error.to_string())?
            } else {
                self.translator
                    .handle_cached_diagnostics(&file_path, collection_options)
                    .await
                    .map_err(|error| error.to_string())?
            };
            if fresh {
                self.attach_diagnostic_fix_handles(&mut result).await;
            }
            result.filters = options;
            if let Ok((path, document_version, content_hash, content)) =
                self.translator.source_snapshot(Path::new(&file_path)).await
            {
                let total_lines = u32::try_from(content.lines().count().max(1)).unwrap_or(u32::MAX);
                result.source_resource = Some(DeferredResourceReference {
                    uri: make_source_uri(
                        &path,
                        1,
                        1,
                        total_lines,
                        1,
                        &content_hash,
                        document_version,
                    )
                    .map_err(|error| error.to_string())?,
                    kind: "source_context".to_owned(),
                    snapshot_hash: content_hash,
                    document_version,
                    total_bytes: Some(content.len()),
                });
            }
            let encoded = serde_json::to_vec(&result.diagnostics)
                .map_err(|error| format!("failed to identify diagnostics snapshot: {error}"))?;
            result.snapshot_identity = Some(format!("{:x}", Sha256::digest(encoded)));
            result.max_bytes = Some(result.filters.byte_limit);
            DiagnosticsPageState {
                file_path,
                fresh,
                result,
            }
        };

        let max_items = state.result.filters.item_limit;
        let max_bytes = state.result.filters.byte_limit;
        let (mut result, continuation) = bounded_diagnostics_page(state, max_items, max_bytes)?;
        if let Some(continuation) = continuation {
            let snapshot_identity = continuation
                .result
                .snapshot_identity
                .clone()
                .ok_or_else(|| "diagnostics page is missing its snapshot identity".to_owned())?;
            let value = serde_json::to_value(continuation)
                .map_err(|error| format!("failed to store diagnostics page: {error}"))?;
            let reference = self
                .deferred_results
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert_scoped(value, snapshot_identity, &scope);
            result.next_cursor = Some(reference.uri);
        }
        debug_assert!(serde_json::to_vec(&result).is_ok_and(|encoded| encoded.len() <= max_bytes));
        Ok(result)
    }

    async fn attach_diagnostic_fix_handles(&mut self, result: &mut DiagnosticsResult) {
        for diagnostic in &mut result.diagnostics {
            let Some(file_path) = diagnostic.context.path.clone() else {
                continue;
            };
            let Ok(actions) = self
                .translator
                .request_code_actions(
                    file_path.clone(),
                    diagnostic.range.start.line,
                    diagnostic.range.start.character,
                    diagnostic.range.end.line,
                    diagnostic.range.end.character,
                    Some(lsp_types::CodeActionKind::QUICKFIX.as_str().to_owned()),
                )
                .await
            else {
                continue;
            };
            diagnostic.context.fix_handles = actions
                .into_iter()
                .map(|action| {
                    self.code_actions
                        .insert(StoredCodeAction {
                            file_path: file_path.clone(),
                            action,
                            created_at: Instant::now(),
                        })
                        .to_string()
                })
                .collect();
        }
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

    async fn document_symbols(
        &self,
        request: DocumentSymbolPageRequest,
    ) -> Result<DocumentSymbolsResult, String> {
        if request.options.limit == 0 || request.options.limit > 1_000 {
            return Err("limit must be between 1 and 1000".to_owned());
        }
        if !(4_096..=1_048_576).contains(&request.max_bytes) {
            return Err("max_bytes must be between 4096 and 1048576".to_owned());
        }

        let scope = self.deferred_scope.as_deref().unwrap_or_default();
        let state = if let Some(page_token) = request.page_token {
            let token = page_token
                .strip_prefix("mcpls-deferred:///")
                .ok_or_else(|| {
                    "page_token must be the next_cursor returned by get_document_symbols".to_owned()
                })?;
            let value = self
                .deferred_results
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .read_scoped(token, scope)?;
            serde_json::from_value(value)
                .map_err(|error| format!("invalid document-symbol page: {error}"))?
        } else {
            let mut result = self
                .translator
                .handle_document_symbols_for_page(
                    request.file_path.clone(),
                    request.options.clone(),
                )
                .await
                .map_err(|error| error.to_string())?;
            let snapshot_identity = result.snapshot_identity.clone().ok_or_else(|| {
                "document-symbol result is missing its snapshot identity".to_owned()
            })?;
            let (path, document_version, content_hash, content) = self
                .translator
                .source_snapshot(Path::new(&request.file_path))
                .await
                .map_err(|error| error.to_string())?;
            if content_hash != snapshot_identity {
                return Err("source changed while preparing the document-symbol page".to_owned());
            }
            let snapshot = document_version.map_or_else(
                || SourceSnapshot::Hash(content_hash),
                SourceSnapshot::Version,
            );
            attach_document_symbol_handles(
                &mut self
                    .symbol_handles
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
                &mut result.symbols,
                &path,
                &snapshot,
                None,
            );
            clear_document_symbol_sources(&mut result.symbols);
            let total_lines = u32::try_from(content.lines().count().max(1)).unwrap_or(u32::MAX);
            let source_resource = DeferredResourceReference {
                uri: make_source_uri(
                    &path,
                    1,
                    1,
                    total_lines,
                    1,
                    &snapshot_identity,
                    document_version,
                )
                .map_err(|error| error.to_string())?,
                kind: "source_context".to_owned(),
                snapshot_hash: snapshot_identity.clone(),
                document_version,
                total_bytes: Some(content.len()),
            };
            DocumentSymbolPageState {
                total: result.total,
                snapshot_identity,
                document_version: result.document_version,
                project_relative_path: result.project_relative_path,
                source_resource,
                filters: request.options.clone(),
                symbols: flatten_document_symbols(result.symbols),
            }
        };

        let max_items = state.filters.limit as usize;
        let (mut result, remaining) =
            bounded_document_symbol_page(state, max_items, request.max_bytes)?;
        if let Some(remaining) = remaining {
            let snapshot_identity = remaining.snapshot_identity.clone();
            let value = serde_json::to_value(remaining)
                .map_err(|error| format!("failed to store document-symbol page: {error}"))?;
            let reference = self
                .deferred_results
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert_scoped(value, snapshot_identity, scope);
            result.next_cursor = Some(reference.uri);
        }
        debug_assert!(
            serde_json::to_vec(&result).is_ok_and(|encoded| encoded.len() <= request.max_bytes)
        );
        Ok(result)
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

    async fn semantic_discovery(
        &self,
        file_path: String,
        line: u32,
        character: u32,
        kind: SemanticDiscoveryKind,
    ) -> Result<SemanticDiscoveryResult, String> {
        let mut result = self
            .translator
            .request_semantic_discovery(file_path, line, character, kind)
            .await
            .map_err(|error| error.to_string())?;
        self.attach_location_handles(&mut result.locations);
        Ok(result)
    }

    async fn workspace_symbol(
        &self,
        query: String,
        kind_filter: Option<String>,
        limit: u32,
        match_mode: WorkspaceSymbolMatchMode,
        scope: WorkspaceSymbolScope,
        include_generated: bool,
    ) -> Result<WorkspaceSymbolResult, String> {
        let mut result = self
            .translator
            .handle_workspace_symbol_with_generated(
                query,
                kind_filter,
                limit,
                match_mode,
                scope,
                include_generated,
            )
            .await
            .map_err(|error| error.to_string())?;
        discard_workspace_symbol_struct_uses(&mut result.symbols);
        result.returned = result.symbols.len();
        self.attach_workspace_symbol_handles(&mut result.symbols)
            .await;
        Ok(result)
    }
    async fn workspace_symbol_page(
        &self,
        request: WorkspaceSymbolPageRequest,
    ) -> Result<WorkspaceSymbolResult, String> {
        if request.max_items == 0 || request.max_items > 1_000 {
            return Err("max_items must be between 1 and 1000".to_owned());
        }
        if !(4_096..=1_048_576).contains(&request.max_bytes) {
            return Err("max_bytes must be between 4096 and 1048576".to_owned());
        }

        let scope = self.deferred_scope.as_deref().unwrap_or_default();
        let state = if let Some(page_token) = request.page_token {
            let token = page_token
                .strip_prefix("mcpls-deferred:///")
                .ok_or_else(|| {
                    "page_token must be the next_cursor returned by workspace_symbol_search"
                        .to_owned()
                })?;
            let value = self
                .deferred_results
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .read_scoped(token, scope)?;
            serde_json::from_value(value)
                .map_err(|error| format!("invalid workspace-symbol page: {error}"))?
        } else {
            let mut result = self
                .translator
                .handle_workspace_symbol_all_with_generated(
                    request.query,
                    request.kind_filter,
                    request.match_mode,
                    request.scope,
                    request.include_generated,
                )
                .await
                .map_err(|error| error.to_string())?;
            discard_workspace_symbol_struct_uses(&mut result.symbols);
            self.attach_workspace_symbol_handles(&mut result.symbols)
                .await;
            let snapshot_identity = workspace_symbol_snapshot_identity(&result.symbols)?;
            WorkspaceSymbolPageState {
                total: result.symbols.len(),
                snapshot_identity,
                symbols: result.symbols,
            }
        };

        let (mut result, remaining) =
            bounded_workspace_symbol_page(state, request.max_items, request.max_bytes)?;
        if let Some(remaining) = remaining {
            let snapshot_identity = remaining.snapshot_identity.clone();
            let value = serde_json::to_value(remaining)
                .map_err(|error| format!("failed to store workspace-symbol page: {error}"))?;
            let reference = self
                .deferred_results
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert_scoped(value, snapshot_identity, scope);
            result.next_cursor = Some(reference.uri);
        }
        debug_assert!(
            serde_json::to_vec(&result).is_ok_and(|encoded| encoded.len() <= request.max_bytes)
        );
        Ok(result)
    }

    async fn workspace_symbol_batch(
        &self,
        request: WorkspaceSymbolBatchRequest,
    ) -> Result<WorkspaceSymbolBatchResult, String> {
        let snapshot_identity = self.workspace_snapshot_identity().await?;
        let mut seen = HashMap::new();
        let mut batch = WorkspaceSymbolBatchResult {
            entries: Vec::with_capacity(request.queries.len()),
            unique_queries: 0,
            provider_requests: 0,
            snapshot_identity,
            cache_hit: false,
            returned: 0,
            truncated: false,
            max_bytes: request.max_bytes,
        };

        for query in request.queries {
            if let Some(&reused_from) = seen.get(&query) {
                batch.entries.push(WorkspaceSymbolBatchEntry {
                    query,
                    result: None,
                    reused_from: Some(reused_from),
                    skipped_by_budget: false,
                });
                trim_workspace_symbol_batch(&mut batch);
                continue;
            }

            let entry_index = batch.entries.len();
            seen.insert(query.clone(), entry_index);
            batch.unique_queries += 1;
            let remaining = request.max_items.saturating_sub(batch.returned);
            if remaining == 0 {
                batch.truncated = true;
                batch.entries.push(WorkspaceSymbolBatchEntry {
                    query,
                    result: None,
                    reused_from: None,
                    skipped_by_budget: true,
                });
                trim_workspace_symbol_batch(&mut batch);
                continue;
            }

            let limit = u32::try_from(remaining).unwrap_or(u32::MAX);
            let cache_key = format!(
                "{}\0{}\0{:?}\0{}\0{:?}\0{:?}",
                batch.snapshot_identity,
                query,
                request.kind_filter,
                request.include_generated,
                request.match_mode,
                request.scope,
            );
            let result = if let Some(result) = self
                .workspace_symbol_results
                .lock()
                .expect("workspace-symbol cache lock poisoned")
                .get(&cache_key)
                .cloned()
            {
                batch.cache_hit = true;
                result
            } else {
                let result = self
                    .workspace_symbol(
                        query.clone(),
                        request.kind_filter.clone(),
                        limit,
                        request.match_mode,
                        request.scope,
                        request.include_generated,
                    )
                    .await?;
                batch.provider_requests += 1;
                if !result.truncated {
                    self.workspace_symbol_results
                        .lock()
                        .expect("workspace-symbol cache lock poisoned")
                        .insert(cache_key, result.clone());
                }
                result
            };
            batch.returned += result.returned;
            batch.truncated |= result.truncated;
            batch.entries.push(WorkspaceSymbolBatchEntry {
                query,
                result: Some(result),
                reused_from: None,
                skipped_by_budget: false,
            });
            trim_workspace_symbol_batch(&mut batch);
        }

        Ok(batch)
    }

    async fn lexical_search(
        &self,
        request: LexicalSearchRequest,
    ) -> Result<LexicalSearchScan, String> {
        const LEXICAL_CONTEXT_BYTES: usize = 16 * 1024;
        let request_identity = lexical_search_request_identity(&request);
        let scope = self.deferred_scope.as_deref().unwrap_or_default();
        let (state, token, offset) = if let Some(page_token) = request.page_token.as_deref() {
            let (token, offset) = parse_lexical_page_cursor(page_token)?;
            let value = self
                .deferred_results
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .read_scoped(token, scope)?;
            let state: LexicalSearchPageState = serde_json::from_value(value)
                .map_err(|error| format!("invalid lexical-search page: {error}"))?;
            if state.request_identity != request_identity {
                return Err("page_token belongs to a different lexical_search request".to_owned());
            }
            (state, token.to_owned(), offset)
        } else {
            let paths = collect_project_paths_filtered(
                self.translator.workspace_roots(),
                request.include_generated,
                request.max_files,
                &request.include_paths,
                &request.exclude_paths,
            )
            .await?;
            let mut matches = Vec::new();
            let mut total_matches: usize = 0;
            let mut scanned_bytes: usize = 0;
            let mut scanned_files: usize = 0;
            let mut source_budget = SourceBudget::new(LEXICAL_CONTEXT_BYTES);
            for path in paths {
                scanned_files += 1;
                let (path, document_version, content_hash, source) =
                    match self.translator.source_snapshot(&path).await {
                        Ok(snapshot) => snapshot,
                        Err(crate::error::Error::Io(error))
                            if error.kind() == std::io::ErrorKind::InvalidData =>
                        {
                            continue;
                        }
                        Err(error) => return Err(error.to_string()),
                    };
                scanned_bytes = scanned_bytes.saturating_add(source.len());
                let ranges = find_matches(
                    &source,
                    &request.query,
                    request.mode,
                    request.case,
                    request.multiline,
                )?;
                total_matches = total_matches.saturating_add(ranges.len());
                let project_relative_path = self
                    .translator
                    .workspace_roots()
                    .iter()
                    .find_map(|root| path.strip_prefix(root).ok())
                    .map(|relative| relative.to_string_lossy().into_owned())
                    .ok_or_else(|| {
                        "lexical search found a path outside its project roots".to_owned()
                    })?;
                for byte_range in ranges {
                    let start =
                        byte_offset_to_position(&source, byte_range.start, PositionEncoding::Utf8)
                            .ok_or_else(|| {
                                "lexical match start is not a valid text position".to_owned()
                            })?;
                    let end =
                        byte_offset_to_position(&source, byte_range.end, PositionEncoding::Utf8)
                            .ok_or_else(|| {
                                "lexical match end is not a valid text position".to_owned()
                            })?;
                    let source_uri = make_source_uri(
                        &path,
                        start.line.saturating_add(1),
                        start.character.saturating_add(1),
                        end.line.saturating_add(1),
                        end.character.saturating_add(1),
                        &content_hash,
                        document_version,
                    )
                    .map_err(|error| error.to_string())?;
                    let source = if request.context_lines == 0 {
                        None
                    } else {
                        Some(
                            self.translator
                                .lexical_source_context(
                                    &path,
                                    crate::bridge::Range {
                                        start: crate::bridge::Position2D {
                                            line: start.line.saturating_add(1),
                                            character: start.character.saturating_add(1),
                                        },
                                        end: crate::bridge::Position2D {
                                            line: end.line.saturating_add(1),
                                            character: end.character.saturating_add(1),
                                        },
                                    },
                                    &mut source_budget,
                                    request.context_lines,
                                )
                                .await,
                        )
                    };
                    matches.push(LexicalSearchMatch {
                        project_relative_path: project_relative_path.clone(),
                        document_version,
                        content_hash: content_hash.clone(),
                        source_uri,
                        source,
                        byte_range,
                    });
                }
            }
            let encoded = serde_json::to_vec(&matches)
                .map_err(|error| format!("failed to identify lexical snapshot: {error}"))?;
            let snapshot_identity = format!("{:x}", Sha256::digest(encoded));
            let state = LexicalSearchPageState {
                matches,
                total_matches,
                scanned_files,
                scanned_bytes,
                snapshot_identity,
                request_identity,
            };
            let value = serde_json::to_value(&state)
                .map_err(|error| format!("failed to store lexical page: {error}"))?;
            let token = self
                .deferred_results
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert_scoped_kind(
                    value,
                    state.snapshot_identity.clone(),
                    scope,
                    "lexical_search_page",
                )
                .uri
                .trim_start_matches("mcpls-deferred:///")
                .to_owned();
            (state, token, 0)
        };

        if offset > state.matches.len() {
            return Err(
                "lexical_search page_token offset is outside the retained result".to_owned(),
            );
        }
        let end = offset
            .saturating_add(request.max_matches)
            .min(state.matches.len());
        Ok(LexicalSearchScan {
            matches: state.matches[offset..end].to_vec(),
            total_matches: state.total_matches,
            scanned_files: state.scanned_files,
            scanned_bytes: state.scanned_bytes,
            offset,
            page_token: token,
            snapshot_identity: state.snapshot_identity,
        })
    }

    async fn lexical_search_batch(
        &self,
        request: LexicalSearchBatchRequest,
    ) -> Result<LexicalSearchBatchResult, String> {
        if request.queries.is_empty() {
            return Err("lexical query batch must not be empty".to_owned());
        }
        let paths = collect_project_paths_filtered(
            self.translator.workspace_roots(),
            request.include_generated,
            request.max_files,
            &request.include_paths,
            &request.exclude_paths,
        )
        .await?;
        let mut files = Vec::with_capacity(paths.len());
        let mut scanned_bytes: usize = 0;
        let mut scanned_files: usize = 0;
        for path in paths {
            scanned_files += 1;
            let (path, document_version, content_hash, source) =
                match self.translator.source_snapshot(&path).await {
                    Ok(snapshot) => snapshot,
                    Err(crate::error::Error::Io(error))
                        if error.kind() == std::io::ErrorKind::InvalidData =>
                    {
                        continue;
                    }
                    Err(error) => return Err(error.to_string()),
                };
            let project_relative_path = self
                .translator
                .workspace_roots()
                .iter()
                .find_map(|root| path.strip_prefix(root).ok())
                .map(|relative| relative.to_string_lossy().into_owned())
                .ok_or_else(|| {
                    "lexical search found a path outside its project roots".to_owned()
                })?;
            scanned_bytes = scanned_bytes.saturating_add(source.len());
            files.push(LexicalFileSnapshot {
                path,
                document_version,
                content_hash,
                source,
                project_relative_path,
            });
        }
        let mut snapshot_hasher = Sha256::new();
        for file in &files {
            snapshot_hasher.update(file.project_relative_path.as_bytes());
            snapshot_hasher.update(file.content_hash.as_bytes());
        }
        let snapshot_identity = format!("{:x}", snapshot_hasher.finalize());
        let mut seen = HashMap::new();
        let mut entries = Vec::with_capacity(request.queries.len());
        let mut returned = 0;
        let mut truncated = false;
        let mut source_budget = SourceBudget::new(16 * 1024);
        for query in request.queries {
            if let Some(&reused_from) = seen.get(&query) {
                entries.push(crate::bridge::lexical::LexicalSearchBatchEntry {
                    query,
                    result: None,
                    reused_from: Some(reused_from),
                    skipped_by_budget: false,
                });
                continue;
            }
            let entry_index = entries.len();
            seen.insert(query.clone(), entry_index);
            let remaining = request.max_matches.saturating_sub(returned);
            if remaining == 0 {
                truncated = true;
                entries.push(crate::bridge::lexical::LexicalSearchBatchEntry {
                    query,
                    result: None,
                    reused_from: None,
                    skipped_by_budget: true,
                });
                continue;
            }
            let mut matches = Vec::new();
            let mut total_matches: usize = 0;
            for file in &files {
                let ranges = find_matches(
                    &file.source,
                    &query,
                    request.mode,
                    request.case,
                    request.multiline,
                )?;
                total_matches = total_matches.saturating_add(ranges.len());
                for byte_range in ranges
                    .into_iter()
                    .take(remaining.saturating_sub(matches.len()))
                {
                    let start = byte_offset_to_position(
                        &file.source,
                        byte_range.start,
                        PositionEncoding::Utf8,
                    )
                    .ok_or_else(|| "lexical match start is not a valid text position".to_owned())?;
                    let end = byte_offset_to_position(
                        &file.source,
                        byte_range.end,
                        PositionEncoding::Utf8,
                    )
                    .ok_or_else(|| "lexical match end is not a valid text position".to_owned())?;
                    let source_uri = make_source_uri(
                        &file.path,
                        start.line.saturating_add(1),
                        start.character.saturating_add(1),
                        end.line.saturating_add(1),
                        end.character.saturating_add(1),
                        &file.content_hash,
                        file.document_version,
                    )
                    .map_err(|error| error.to_string())?;
                    let source = if request.context_lines == 0 {
                        None
                    } else {
                        Some(
                            self.translator
                                .lexical_source_context(
                                    &file.path,
                                    crate::bridge::Range {
                                        start: crate::bridge::Position2D {
                                            line: start.line.saturating_add(1),
                                            character: start.character.saturating_add(1),
                                        },
                                        end: crate::bridge::Position2D {
                                            line: end.line.saturating_add(1),
                                            character: end.character.saturating_add(1),
                                        },
                                    },
                                    &mut source_budget,
                                    request.context_lines,
                                )
                                .await,
                        )
                    };
                    matches.push(LexicalSearchMatch {
                        project_relative_path: file.project_relative_path.clone(),
                        document_version: file.document_version,
                        content_hash: file.content_hash.clone(),
                        source_uri,
                        source,
                        byte_range,
                    });
                }
            }
            let query_identity = format!(
                "{:x}",
                Sha256::digest(serde_json::to_vec(&matches).unwrap_or_default())
            );
            let query_truncated = total_matches > matches.len();
            truncated |= query_truncated;
            returned += matches.len();
            entries.push(crate::bridge::lexical::LexicalSearchBatchEntry {
                query,
                result: Some(crate::bridge::lexical::LexicalSearchResult {
                    returned: matches.len(),
                    total: total_matches,
                    remaining: total_matches.saturating_sub(matches.len()),
                    scanned_files,
                    scanned_bytes,
                    snapshot_identity: format!("{snapshot_identity}:{query_identity}"),
                    max_bytes: request.max_bytes,
                    truncated: query_truncated,
                    next_cursor: None,
                    matches,
                }),
                reused_from: None,
                skipped_by_budget: false,
            });
        }
        Ok(LexicalSearchBatchResult {
            unique_queries: seen.len(),
            entries,
            scanned_files,
            scanned_bytes,
            returned,
            truncated,
            max_matches: request.max_matches,
            max_bytes: request.max_bytes,
            snapshot_identity,
        })
    }

    async fn workspace_snapshot_identity(&self) -> Result<String, String> {
        let mut paths = Vec::new();
        for root in self.translator.workspace_roots() {
            for entry in WalkBuilder::new(root)
                .standard_filters(true)
                .build()
                .flatten()
            {
                if entry.file_type().is_some_and(|kind| kind.is_file()) {
                    paths.push(entry.into_path());
                }
            }
        }
        paths.sort_unstable();
        let mut hasher = Sha256::new();
        for path in paths {
            let (_, version, content_hash, _) = self
                .translator
                .source_snapshot(&path)
                .await
                .map_err(|error| error.to_string())?;
            hasher.update(path.as_os_str().as_encoded_bytes());
            hasher.update(version.unwrap_or_default().to_le_bytes());
            hasher.update(content_hash.as_bytes());
        }
        Ok(format!("{:x}", hasher.finalize()))
    }

    async fn workspace_symbol_in_path(
        &self,
        query: String,
        kind_filter: Option<String>,
        limit: u32,
        match_mode: WorkspaceSymbolMatchMode,
        scope: WorkspaceSymbolScope,
        path: &Path,
    ) -> Result<WorkspaceSymbolResult, String> {
        let mut result = self
            .translator
            .handle_workspace_symbol_in_path(query, kind_filter, limit, match_mode, scope, path)
            .await
            .map_err(|error| error.to_string())?;
        discard_workspace_symbol_struct_uses(&mut result.symbols);
        result.returned = result.symbols.len();
        self.attach_workspace_symbol_handles(&mut result.symbols)
            .await;
        Ok(result)
    }

    // One actor-owned operation intentionally makes the snapshot boundary visible.
    #[allow(clippy::too_many_lines)]
    async fn inspect_symbol(
        &self,
        mut request: InspectSymbolRequest,
    ) -> Result<InspectSymbolResult, String> {
        use crate::bridge::{
            InspectCalls, InspectSection, InspectSymbolResolution, InspectSymbolSectionKind,
            InspectSymbolSections,
        };

        request.budget.max_bytes = request
            .budget
            .max_bytes
            .min(crate::bridge::translator::INSPECT_SYMBOL_RESULT_MAX_BYTES);

        let (resolution, target) = if let Some(handle) = request.symbol_handle.clone() {
            let target = match self.resolve_symbol_target(&handle).await {
                Ok(target) => target,
                Err(error) if error.starts_with("stale_symbol_handle:") => {
                    return Ok(InspectSymbolResult {
                        resolution: InspectSymbolResolution::Stale {
                            symbol_handle: handle,
                            reason: error,
                            retryable: true,
                        },
                        sections: InspectSymbolSections::default(),
                        budget: request.budget,
                        returned_bytes: 0,
                        truncated: false,
                    });
                }
                Err(error) => return Err(error),
            };
            (
                InspectSymbolResolution::Selected {
                    symbol: None,
                    symbol_handle: Some(handle),
                },
                Some((
                    target.file_path.to_string_lossy().into_owned(),
                    target.line,
                    target.character,
                    None,
                )),
            )
        } else {
            let query = request
                .query
                .as_ref()
                .filter(|query| !query.is_empty())
                .ok_or_else(|| "query or symbol_handle is required".to_owned())?;
            let mut result = if let Some(path) = request.path.as_ref() {
                let path = PathBuf::from(path);
                let path = if path.is_absolute() {
                    path
                } else {
                    self.translator
                        .workspace_roots()
                        .first()
                        .ok_or_else(|| "project has no workspace root".to_owned())?
                        .join(path)
                };
                let path = dunce::canonicalize(path).map_err(|error| error.to_string())?;
                self.workspace_symbol_in_path(
                    query.clone(),
                    request.kind.clone(),
                    request.candidate_limit,
                    WorkspaceSymbolMatchMode::Exact,
                    WorkspaceSymbolScope::Project,
                    &path,
                )
                .await?
            } else {
                self.workspace_symbol(
                    query.clone(),
                    request.kind.clone(),
                    request.candidate_limit,
                    WorkspaceSymbolMatchMode::Exact,
                    WorkspaceSymbolScope::Project,
                    false,
                )
                .await?
            };
            result.symbols.retain(|symbol| {
                request.container.as_ref().is_none_or(|container| {
                    symbol.container_name.as_deref() == Some(container.as_str())
                })
            });
            match result.symbols.len() {
                0 => (InspectSymbolResolution::NotFound, None),
                1 => {
                    let symbol = result.symbols.remove(0);
                    let symbol_range = symbol.location.range.clone();
                    let target = if let Some(handle) = symbol.location.symbol_handle.as_ref() {
                        let stored = match self.resolve_symbol_target(handle).await {
                            Ok(stored) => stored,
                            Err(error) if error.starts_with("stale_symbol_handle:") => {
                                return Ok(InspectSymbolResult {
                                    resolution: InspectSymbolResolution::Stale {
                                        symbol_handle: handle.clone(),
                                        reason: error,
                                        retryable: true,
                                    },
                                    sections: InspectSymbolSections::default(),
                                    budget: request.budget,
                                    returned_bytes: 0,
                                    truncated: false,
                                });
                            }
                            Err(error) => return Err(error),
                        };
                        Some((
                            stored.file_path.to_string_lossy().into_owned(),
                            stored.line,
                            stored.character,
                            Some(symbol_range),
                        ))
                    } else {
                        symbol.location.path.clone().map(|path| {
                            (
                                path,
                                symbol.location.range.start.line,
                                symbol.location.range.start.character,
                                Some(symbol_range),
                            )
                        })
                    };
                    (
                        InspectSymbolResolution::Selected {
                            symbol_handle: symbol.location.symbol_handle.clone(),
                            symbol: Some(Box::new(symbol)),
                        },
                        target,
                    )
                }
                _ => (
                    InspectSymbolResolution::Ambiguous {
                        candidates: result.symbols,
                    },
                    None,
                ),
            }
        };

        let mut sections = InspectSymbolSections::default();
        let Some((file_path, line, character, symbol_range)) = target else {
            let mut result = InspectSymbolResult {
                resolution,
                sections,
                budget: request.budget,
                returned_bytes: 0,
                truncated: false,
            };
            while serde_json::to_vec(&result).map_or(usize::MAX, |json| json.len())
                > result.budget.max_bytes
            {
                let InspectSymbolResolution::Ambiguous { candidates } = &mut result.resolution
                else {
                    break;
                };
                if candidates.len() <= 1 || candidates.pop().is_none() {
                    break;
                }
                result.truncated = true;
            }
            update_inspect_symbol_byte_count(&mut result);
            return Ok(result);
        };
        let limits = SemanticResultLimits {
            total: request.budget.max_items,
            per_file: request.budget.max_items,
            per_symbol: request.budget.max_items,
        };
        let wants_hover = request.wants(InspectSymbolSectionKind::Declaration)
            || request.wants(InspectSymbolSectionKind::Hover);
        let wants_definitions = request.wants(InspectSymbolSectionKind::Definitions);
        let wants_implementations = request.wants(InspectSymbolSectionKind::Implementations);
        let wants_references = request.wants(InspectSymbolSectionKind::References);
        let wants_calls = request.wants(InspectSymbolSectionKind::Calls);
        let wants_tests = request.wants(InspectSymbolSectionKind::Tests);
        let wants_runnables = request.wants(InspectSymbolSectionKind::Runnables);
        let wants_diagnostics = request.wants(InspectSymbolSectionKind::Diagnostics);
        let diagnostics_options = DiagnosticOptions {
            item_limit: request.budget.max_items,
            byte_limit: request.budget.max_bytes,
            ..DiagnosticOptions::default()
        };

        let hover_request = Box::pin(async {
            self.translator
                .handle_hover(file_path.clone(), line, character)
                .await
                .map_err(|error| error.to_string())
        });
        let definitions_request = Box::pin(async {
            self.translator
                .handle_definition(file_path.clone(), line, character)
                .await
                .map_err(|error| error.to_string())
        });
        let implementations_request = Box::pin(async {
            self.translator
                .handle_implementation(file_path.clone(), line, character)
                .await
                .map_err(|error| error.to_string())
        });
        let references_request = Box::pin(async {
            self.translator
                .handle_references_page(file_path.clone(), line, character, true, limits, Some(0))
                .await
                .map_err(|error| error.to_string())
        });
        let calls_request = Box::pin(async {
            let prepared = self
                .translator
                .handle_call_hierarchy_prepare(file_path.clone(), line, character)
                .await
                .map_err(|error| error.to_string())?;
            let Some(first) = prepared.items.first() else {
                return Ok(missing_call_hierarchy_item());
            };
            let provider = prepared.provider;
            let item = serde_json::to_value(first).map_err(|error| error.to_string())?;
            let (incoming, outgoing) = tokio::join!(
                self.translator.handle_incoming_calls(item.clone(), limits),
                self.translator.handle_outgoing_calls(item, limits),
            );
            let incoming = incoming.map_err(|error| error.to_string())?;
            let outgoing = outgoing.map_err(|error| error.to_string())?;
            let total = incoming.total_calls + outgoing.total_calls;
            let returned = incoming.returned_calls + outgoing.returned_calls;
            let truncated = incoming.truncated || outgoing.truncated;
            Ok(InspectSection::available(
                provider,
                total,
                returned,
                truncated,
                InspectCalls { incoming, outgoing },
            ))
        });
        let tests_request = Box::pin(async {
            self.translator
                .request_semantic_discovery(
                    file_path.clone(),
                    line,
                    character,
                    SemanticDiscoveryKind::RelatedTests,
                )
                .await
                .map_err(|error| error.to_string())
        });
        let runnables_request = Box::pin(async {
            self.translator
                .request_semantic_discovery(
                    file_path.clone(),
                    line,
                    character,
                    SemanticDiscoveryKind::Runnables,
                )
                .await
                .map_err(|error| error.to_string())
        });
        let diagnostics_request = Box::pin(async {
            self.translator
                .handle_cached_diagnostics(&file_path, diagnostics_options)
                .await
                .map_err(|error| error.to_string())
        });
        // A newly opened document can make the first semantic request receive
        // ContentModified while its language server settles. Complete the
        // declaration preflight before fanning out independent sections so
        // they share that synchronization instead of retrying together.
        let hover = inspect_if_requested(wants_hover, hover_request).await;
        let (definitions, implementations, references, calls, tests, runnables, diagnostics) = tokio::join!(
            inspect_if_requested(wants_definitions, definitions_request),
            inspect_if_requested(wants_implementations, implementations_request),
            inspect_if_requested(wants_references, references_request),
            inspect_if_requested(wants_calls, calls_request),
            inspect_if_requested(wants_tests, tests_request),
            inspect_if_requested(wants_runnables, runnables_request),
            inspect_if_requested(wants_diagnostics, diagnostics_request),
        );

        if let Some(hover) = hover {
            match hover {
                Ok(hover) => {
                    let mut hover = hover;
                    let target = hover.range.as_ref().map_or((line, character), |range| {
                        (range.start.line, range.start.character)
                    });
                    hover.symbol_handle = self.source_handle(&hover.source, target.0, target.1);
                    if request.wants(InspectSymbolSectionKind::Declaration) {
                        sections.declaration = InspectSection::available(
                            hover.provider.clone(),
                            1,
                            1,
                            hover.truncated,
                            hover.source.clone(),
                        );
                    }
                    if request.wants(InspectSymbolSectionKind::Hover) {
                        sections.hover = InspectSection::available(
                            hover.provider.clone(),
                            1,
                            1,
                            hover.truncated,
                            hover,
                        );
                    }
                }
                Err(error) => {
                    if request.wants(InspectSymbolSectionKind::Declaration) {
                        sections.declaration = InspectSection::unavailable(error.clone());
                    }
                    if request.wants(InspectSymbolSectionKind::Hover) {
                        sections.hover = InspectSection::unavailable(error);
                    }
                }
            }
        }
        if let Some(definitions) = definitions {
            sections.definitions = match definitions {
                Ok(mut result) => {
                    self.attach_location_handles(&mut result.locations);
                    InspectSection::available(
                        result.provider.clone(),
                        result.locations.len(),
                        result.locations.len(),
                        result.truncated,
                        result,
                    )
                }
                Err(error) => InspectSection::unavailable(error),
            };
        }
        if let Some(implementations) = implementations {
            sections.implementations = match implementations {
                Ok(mut result) => {
                    self.attach_location_handles(&mut result.locations);
                    InspectSection::available(
                        result.provider.clone(),
                        result.locations.len(),
                        result.locations.len(),
                        result.truncated,
                        result,
                    )
                }
                Err(error) => InspectSection::unavailable(error),
            };
        }
        if let Some(references) = references {
            sections.references = match references {
                Ok(mut result) => {
                    for group in &mut result.groups {
                        for reference in &mut group.references {
                            self.attach_reference_handle(reference);
                        }
                    }
                    if let Some(declaration) = result.declaration.as_mut() {
                        self.attach_location_handle(declaration);
                    }
                    InspectSection::available(
                        result.provider.clone(),
                        result.total_references,
                        result.returned_references,
                        result.truncated,
                        result,
                    )
                }
                Err(error) => InspectSection::unavailable(error),
            };
        }
        if let Some(calls) = calls {
            sections.calls = match calls {
                Ok(mut section) => {
                    if let Some(calls) = section.data.as_mut() {
                        for call in &mut calls.incoming.calls {
                            let item = &mut call.from;
                            item.symbol_handle = item.source.as_ref().and_then(|source| {
                                self.source_handle(
                                    source,
                                    item.selection_range.start.line,
                                    item.selection_range.start.character,
                                )
                            });
                        }
                        for call in &mut calls.outgoing.calls {
                            let item = &mut call.to;
                            item.symbol_handle = item.source.as_ref().and_then(|source| {
                                self.source_handle(
                                    source,
                                    item.selection_range.start.line,
                                    item.selection_range.start.character,
                                )
                            });
                        }
                    }
                    section
                }
                Err(error) => InspectSection::unavailable(error),
            };
        }
        for (result, section) in [
            (tests, &mut sections.tests),
            (runnables, &mut sections.runnables),
        ] {
            let Some(result) = result else { continue };
            *section = match result {
                Ok(result) if !result.supported => InspectSection::unsupported(
                    result.provider,
                    "provider does not support section",
                ),
                Ok(mut result) => {
                    self.attach_location_handles(&mut result.locations);
                    InspectSection::available(
                        result.provider.clone(),
                        result.runnables.len(),
                        result.runnables.len(),
                        result.truncated,
                        result,
                    )
                }
                Err(error) => InspectSection::unavailable(error),
            };
        }
        if let Some(diagnostics) = diagnostics {
            sections.diagnostics = match diagnostics {
                Ok(mut result) => {
                    result.diagnostics.retain(|diagnostic| {
                        symbol_range
                            .as_ref()
                            .map_or(diagnostic.range.start.line == line, |range| {
                                diagnostic.range.start.line <= range.end.line
                                    && diagnostic.range.end.line >= range.start.line
                            })
                    });
                    let relevant = result.diagnostics.len();
                    result.total_diagnostics = relevant;
                    result.returned_diagnostics = relevant;
                    result.total_groups = relevant;
                    result.returned_groups = relevant;
                    result.omitted_groups = 0;
                    InspectSection::available(
                        "lsp/diagnostics",
                        relevant,
                        relevant,
                        result.truncated,
                        result,
                    )
                }
                Err(error) => InspectSection::unavailable(error),
            };
        }

        let mut result = InspectSymbolResult {
            resolution,
            sections,
            budget: request.budget,
            returned_bytes: 0,
            truncated: false,
        };
        let snapshot_hash = self
            .translator
            .source_snapshot(Path::new(&file_path))
            .await
            .map_or_else(
                |_| format!("generation:{}", self.generation),
                |(_, _, hash, _)| hash,
            );
        macro_rules! drop_section_if_over_budget {
            ($field:ident) => {
                if serde_json::to_vec(&result).map_or(usize::MAX, |json| json.len())
                    > result.budget.max_bytes
                    && result.sections.$field.completeness
                        != crate::bridge::InspectSectionCompleteness::NotRequested
                    && result.sections.$field.completeness
                        != crate::bridge::InspectSectionCompleteness::Deferred
                {
                    self.defer_inspect_section(&mut result.sections.$field, &snapshot_hash);
                    result.truncated = true;
                }
            };
        }
        drop_section_if_over_budget!(runnables);
        drop_section_if_over_budget!(hover);
        drop_section_if_over_budget!(definitions);
        drop_section_if_over_budget!(diagnostics);
        drop_section_if_over_budget!(tests);
        drop_section_if_over_budget!(references);
        drop_section_if_over_budget!(calls);
        drop_section_if_over_budget!(implementations);
        drop_section_if_over_budget!(declaration);
        if serde_json::to_vec(&result).map_or(usize::MAX, |json| json.len())
            > result.budget.max_bytes
            && let InspectSymbolResolution::Selected { symbol, .. } = &mut result.resolution
        {
            *symbol = None;
            result.truncated = true;
        }
        update_inspect_symbol_byte_count(&mut result);
        Ok(result)
    }

    async fn collect_inspect_symbol_batch(
        &self,
        request: InspectSymbolBatchRequest,
        scope: &str,
    ) -> Result<InspectSymbolBatchSnapshot, String> {
        let target_count = request.targets.len();
        if target_count == 0
            || target_count > crate::bridge::translator::INSPECT_SYMBOL_BATCH_MAX_TARGETS
        {
            return Err("between 1 and 16 symbol targets are required".to_owned());
        }
        let identity_bytes = serde_json::to_vec(&request.targets)
            .map_err(|error| error.to_string())?
            .len();
        let available_bytes = request.budget.max_bytes.saturating_sub(
            identity_bytes
                + crate::bridge::translator::INSPECT_SYMBOL_BATCH_RESPONSE_OVERHEAD_BYTES,
        );
        let target_budget = crate::bridge::InspectSymbolBudget {
            max_bytes: (available_bytes / target_count)
                .min(crate::bridge::translator::INSPECT_SYMBOL_BATCH_MAX_ENTRY_BYTES),
            max_items: request.budget.max_items / target_count,
        };
        if target_budget.max_bytes
            < crate::bridge::translator::INSPECT_SYMBOL_BATCH_MIN_BYTES_PER_TARGET
            || target_budget.max_items == 0
        {
            return Err("batch budget is too small for every symbol target".to_owned());
        }

        let inspections = request.targets.into_iter().map(|target| {
            let inspect_request = InspectSymbolRequest {
                symbol_handle: target.symbol_handle.clone(),
                query: target.query.clone(),
                kind: target.kind.clone(),
                path: target.path.clone(),
                container: target.container.clone(),
                candidate_limit: request.candidate_limit,
                sections: request.sections.clone(),
                budget: target_budget,
            };
            async move {
                match Box::pin(self.inspect_symbol(inspect_request)).await {
                    Ok(result) => InspectSymbolBatchEntry {
                        target,
                        result: Some(result),
                        error: None,
                        resource: None,
                    },
                    Err(error) => InspectSymbolBatchEntry {
                        target,
                        result: None,
                        error: Some(error),
                        resource: None,
                    },
                }
            }
        });
        let mut entries = futures::future::join_all(inspections).await;
        let truncated = entries
            .iter()
            .filter_map(|entry| entry.result.as_ref())
            .any(|result| result.truncated);
        let encoded = serde_json::to_vec(&entries)
            .map_err(|error| format!("failed to identify inspect batch snapshot: {error}"))?;
        let snapshot_identity = format!("{:x}", Sha256::digest(encoded));
        for entry in &mut entries {
            if serde_json::to_vec(entry).map_or(usize::MAX, |json| json.len())
                <= crate::bridge::translator::INSPECT_SYMBOL_BATCH_MAX_ENTRY_BYTES
            {
                continue;
            }
            let value = serde_json::to_value(&*entry)
                .map_err(|error| format!("failed to defer inspect batch entry: {error}"))?;
            let resource = self
                .deferred_results
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert_scoped(value, snapshot_identity.clone(), scope);
            entry.result = None;
            entry.error = None;
            entry.resource = Some(resource);
        }
        Ok(InspectSymbolBatchSnapshot {
            inspections_started: entries.len(),
            entries,
            snapshot_identity,
            truncated,
            max_items: request.budget.max_items,
        })
    }

    async fn inspect_symbol_batch(
        &self,
        request: InspectSymbolBatchRequest,
    ) -> Result<InspectSymbolBatchResult, String> {
        let scope = self.deferred_scope.as_deref().unwrap_or_default();
        if let Some(page_token) = request.page_token.as_deref() {
            if !request.targets.is_empty() {
                return Err("targets must be empty when page_token is supplied".to_owned());
            }
            let (token, offset) = parse_inspect_symbol_batch_cursor(page_token)?;
            let snapshot = self
                .inspect_symbol_batch_pages
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .read(token, scope)?;
            return bounded_inspect_symbol_batch_page(&snapshot, token, offset);
        }

        let snapshot = self.collect_inspect_symbol_batch(request, scope).await?;
        let token = self
            .inspect_symbol_batch_pages
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(snapshot.clone(), scope);
        let result = bounded_inspect_symbol_batch_page(&snapshot, &token, 0)?;
        if result.next_cursor.is_none() {
            self.inspect_symbol_batch_pages
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&token);
        }
        Ok(result)
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
        self.preview_edit(project_id, edit, encoding, root).await
    }

    async fn prepare_call_hierarchy(
        &self,
        file_path: String,
        line: u32,
        character: u32,
        page_token: Option<String>,
    ) -> Result<CallHierarchyPrepareResult, String> {
        let (provider, kind, total_items, truncated, snapshot_hash, items) =
            if let Some(page_token) = page_token {
                let token = page_token
                    .strip_prefix("mcpls-deferred:///")
                    .ok_or_else(|| "invalid call hierarchy page token".to_owned())?;
                let scope = self.deferred_scope.as_deref().unwrap_or_default();
                let value = self
                    .deferred_results
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .read_scoped(token, scope)?;
                let provider = value
                    .get("provider")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| "invalid call hierarchy page payload".to_owned())?
                    .to_owned();
                let kind = serde_json::from_value(
                    value
                        .get("kind")
                        .cloned()
                        .ok_or_else(|| "invalid call hierarchy page payload".to_owned())?,
                )
                .map_err(|error| format!("invalid call hierarchy page kind: {error}"))?;
                let total_items = value
                    .get("total_items")
                    .and_then(serde_json::Value::as_u64)
                    .and_then(|value| usize::try_from(value).ok())
                    .ok_or_else(|| "invalid call hierarchy page count".to_owned())?;
                let truncated = value
                    .get("truncated")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false);
                let snapshot_hash = value
                    .get("snapshot_hash")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let items = serde_json::from_value(
                    value
                        .get("items")
                        .cloned()
                        .ok_or_else(|| "invalid call hierarchy page items".to_owned())?,
                )
                .map_err(|error| format!("invalid call hierarchy page items: {error}"))?;
                (provider, kind, total_items, truncated, snapshot_hash, items)
            } else {
                let mut result = self
                    .translator
                    .handle_call_hierarchy_prepare(file_path, line, character)
                    .await
                    .map_err(|error| error.to_string())?;
                for item in &mut result.items {
                    item.symbol_handle = item.source.as_ref().and_then(|source| {
                        self.source_handle(
                            source,
                            item.selection_range.start.line,
                            item.selection_range.start.character,
                        )
                    });
                }
                let snapshot_hash = call_hierarchy_snapshot_hash(&result.items);
                (
                    result.provider,
                    result.kind,
                    result.total_items,
                    result.truncated,
                    snapshot_hash,
                    result.items,
                )
            };

        let (items, remaining) = page_items(items, CALL_HIERARCHY_PAGE_SIZE);
        let next_cursor = remaining.map(|items| {
            let value = serde_json::json!({
                "provider": provider,
                "kind": kind,
                "total_items": total_items,
                "truncated": truncated,
                "snapshot_hash": snapshot_hash,
                "items": items,
            });
            let scope = self.deferred_scope.as_deref().unwrap_or_default();
            self.deferred_results
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert_scoped(value, snapshot_hash.clone(), scope)
                .uri
        });

        let has_next_page = next_cursor.is_some();

        Ok(CallHierarchyPrepareResult {
            provider,
            kind,
            total_items,
            returned_items: items.len(),
            next_cursor,
            truncated: truncated || has_next_page,
            items,
        })
    }

    async fn incoming_calls(
        &self,
        item: serde_json::Value,
        limits: SemanticResultLimits,
    ) -> Result<IncomingCallsResult, String> {
        let mut result = self
            .translator
            .handle_incoming_calls(item, limits)
            .await
            .map_err(|error| error.to_string())?;
        for call in &mut result.calls {
            let item = &mut call.from;
            item.symbol_handle = item.source.as_ref().and_then(|source| {
                self.source_handle(
                    source,
                    item.selection_range.start.line,
                    item.selection_range.start.character,
                )
            });
        }
        Ok(result)
    }

    async fn outgoing_calls(
        &self,
        item: serde_json::Value,
        limits: SemanticResultLimits,
    ) -> Result<OutgoingCallsResult, String> {
        let mut result = self
            .translator
            .handle_outgoing_calls(item, limits)
            .await
            .map_err(|error| error.to_string())?;
        for call in &mut result.calls {
            let item = &mut call.to;
            item.symbol_handle = item.source.as_ref().and_then(|source| {
                self.source_handle(
                    source,
                    item.selection_range.start.line,
                    item.selection_range.start.character,
                )
            });
        }
        Ok(result)
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
        let mut result = self
            .translator
            .handle_implementation(file_path, line, character)
            .await
            .map_err(|error| error.to_string())?;
        self.attach_location_handles(&mut result.locations);
        Ok(result)
    }

    async fn go_to_type_definition(
        &self,
        file_path: String,
        line: u32,
        character: u32,
    ) -> Result<LocationsResult, String> {
        let mut result = self
            .translator
            .handle_type_definition(file_path, line, character)
            .await
            .map_err(|error| error.to_string())?;
        self.attach_location_handles(&mut result.locations);
        Ok(result)
    }

    async fn cached_diagnostics(
        &mut self,
        file_path: &str,
        options: DiagnosticOptions,
    ) -> Result<DiagnosticsResult, String> {
        self.diagnostics_page(file_path.to_owned(), options, false)
            .await
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

    fn source_path_is_authorized(&self, path: &Path) -> bool {
        self.translator.source_path_is_authorized(path)
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
        cancellation: CancellationToken,
    ) -> Result<ProjectActivation, String> {
        self.translator
            .activate_project_with_roots_cancelled(roots, cancellation)
            .await
            .map_err(|error| error.to_string())
    }

    async fn add_workspace_root(
        &mut self,
        root: PathBuf,
        status: ProjectStatus,
        cancellation: CancellationToken,
    ) -> Result<ProjectActivation, String> {
        if status == ProjectStatus::Degraded
            && !self.has_active_workspace_roots(self.translator.workspace_roots())
        {
            let mut roots = self.translator.workspace_roots().to_vec();
            if !roots.contains(&root) {
                roots.push(root);
            }
            return self.activate_workspace_roots(roots, cancellation).await;
        }
        self.translator
            .add_workspace_root_cancelled(root, cancellation)
            .await
            .map_err(|error| error.to_string())
    }

    async fn restart(
        &mut self,
        cancellation: CancellationToken,
    ) -> Result<ProjectActivation, String> {
        let roots = self.translator.workspace_roots().to_vec();
        if roots.is_empty() {
            return Ok(ProjectActivation::ready());
        }
        if self.translator.configured_language_ids().is_empty() {
            return Ok(ProjectActivation::ready());
        }
        self.shutdown().await?;
        self.activate_workspace_roots(roots, cancellation).await
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

#[allow(clippy::large_futures)]
async fn recover_project_after_server_exit(
    actor_sender: &mpsc::WeakSender<ProjectRequest>,
    channels: &ProjectActorChannels,
    state: &mut ProjectState,
    runtime: &mut ProjectRuntime,
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
        if !channels.gate.is_accepting() {
            return;
        }
        tokio::select! {
            () = tokio::time::sleep(attempt.delay) => {}
            () = channels.gate.wait_for_rejection() => return,
        }
        if !channels.gate.is_accepting() {
            return;
        }
        let cancellation = CancellationToken::new();
        match run_cancellable_transition(
            &channels.gate,
            cancellation.clone(),
            runtime.restart(cancellation),
        )
        .await
        {
            Ok(notification_receivers) => {
                state.last_error = None;
                mark_project_started(
                    notification_receivers,
                    actor_sender,
                    channels,
                    state,
                    runtime,
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

#[allow(clippy::large_futures)]
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
            let _recovery_guard = match residency {
                Some(residency) => {
                    if let Some(guard) = residency.try_acquire_existing_for_recovery() {
                        Some(guard)
                    } else {
                        Some(
                            residency
                                .controller
                                .acquire_for(residency.group, RustResidencyMode::Activate)
                                .await,
                        )
                    }
                }
                None => None,
            };
            recover_project_after_server_exit(actor_sender, channels, state, runtime).await;
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
    fn try_acquire_existing(&self) -> Option<residency::RustResidencyGuard> {
        self.controller.try_acquire_existing(self.group)
    }

    fn try_acquire_existing_for_recovery(&self) -> Option<residency::RustResidencyGuard> {
        self.controller
            .try_acquire_existing_for_recovery(self.group)
    }

    fn remove(&self) {
        self.controller.remove(self.group);
    }

    async fn resident_request(
        &self,
        request: ProjectRequest,
        mode: RustResidencyMode,
    ) -> ProjectRequest {
        let guard = self.controller.acquire_for(self.group, mode).await;
        ProjectRequest::Resident {
            request: Box::new(request),
            guard,
        }
    }

    fn touch_request(&self, request: ProjectRequest) -> ProjectRequest {
        let Some(guard) = self.try_acquire_existing() else {
            return request;
        };
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
    spawn_project_actor_with_deferred_results(
        capacity,
        translator,
        edit_safety,
        residency,
        std::sync::Arc::new(std::sync::Mutex::new(DeferredResultStore::new())),
    )
}

fn spawn_project_actor_with_deferred_results(
    capacity: usize,
    translator: Translator,
    edit_safety: Option<EditSafetyConfig>,
    residency: Option<ProjectResidency>,
    deferred_results: std::sync::Arc<std::sync::Mutex<DeferredResultStore>>,
) -> ProjectHandle {
    spawn_project_actor_with_deferred_results_scoped(
        capacity,
        translator,
        edit_safety,
        residency,
        deferred_results,
        None,
    )
}

fn spawn_project_actor_with_deferred_results_scoped(
    capacity: usize,
    translator: Translator,
    edit_safety: Option<EditSafetyConfig>,
    residency: Option<ProjectResidency>,
    deferred_results: std::sync::Arc<std::sync::Mutex<DeferredResultStore>>,
    deferred_scope: Option<String>,
) -> ProjectHandle {
    let (sender, receiver) = mpsc::channel(capacity.max(1));
    let actor_sender = sender.downgrade();
    let gate = ProjectRequestGate::new();
    if let Some(residency) = &residency {
        residency
            .controller
            .register(residency.group, actor_sender.clone());
    }
    let sender = residency.as_ref().map_or_else(
        || ProjectRequestSender::with_gate(sender.clone(), None, gate.clone()),
        |residency| {
            ProjectRequestSender::with_gate(sender.clone(), Some(residency.clone()), gate.clone())
        },
    );
    let runtime = ProjectRuntime::with_deferred_results_scoped(
        translator,
        edit_safety,
        deferred_results,
        deferred_scope,
    );
    let initial_state = ProjectState::new(ProjectStatus::Starting, runtime.summary());
    let (status_tx, status_rx) = watch::channel(ProjectStatus::Starting);
    let (state_tx, state_rx) = watch::channel(initial_state.clone());
    let (event_tx, _) = broadcast::channel(256);
    let event_sender = event_tx.clone();
    let event_history = std::sync::Arc::new(std::sync::Mutex::new(ProjectEventHistory::new(256)));
    let channels = ProjectActorChannels {
        status_tx,
        state_tx,
        event_tx,
        event_history: std::sync::Arc::clone(&event_history),
        gate,
    };
    tokio::spawn(run_project_actor(
        receiver,
        actor_sender,
        channels,
        initial_state,
        runtime,
        residency,
    ));
    ProjectHandle {
        sender,
        status: status_rx,
        state: state_rx,
        events: event_sender,
        event_history,
    }
}

struct ProjectActorChannels {
    status_tx: watch::Sender<ProjectStatus>,
    state_tx: watch::Sender<ProjectState>,
    event_tx: broadcast::Sender<ProjectEvent>,
    event_history: std::sync::Arc<std::sync::Mutex<ProjectEventHistory>>,
    gate: ProjectRequestGate,
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
        if status == ProjectStatus::Dormant {
            state
                .dormancy
                .get_or_insert(ProjectDormancy::new(ProjectDormancyReason::Restored, None));
        } else {
            state.dormancy = None;
        }
        let _ = self.status_tx.send(status);
        self.publish_state(state);
        self.publish(ProjectEvent::StatusChanged {
            status,
            last_error: state.last_error.clone(),
        });
    }

    fn publish_state(&self, state: &ProjectState) {
        self.state_tx.send_replace(state.clone());
    }

    fn publish_failure(&self, state: &mut ProjectState, error: impl Into<String>) {
        state.last_error = Some(error.into());
        self.publish_status(state, ProjectStatus::Failed);
    }
}

#[allow(clippy::large_futures)]
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
        let (request, timing) = request.into_timed();
        if matches!(&request, ProjectRequest::Shutdown { .. }) {
            while runtime.active_edit_workers > 0 {
                let Some(next) = next_project_request(&mut receiver).await else {
                    break;
                };
                let (next, _residency_guard) = next.into_resident();
                let (next, timing) = next.into_timed();
                let stop = handle_timed_project_request(
                    next,
                    timing,
                    &actor_sender,
                    &channels,
                    &mut state,
                    &mut runtime,
                    residency.as_ref(),
                )
                .await;
                state.sync_runtime(&runtime);
                channels.publish_state(&state);
                if stop {
                    break;
                }
            }
        }
        let resumes_runtime = request.resumes_rust_runtime();
        if residency.is_some()
            && resumes_runtime
            && !runtime.activation_is_reusable(state.status, runtime.translator.workspace_roots())
        {
            resume_project_runtime(&actor_sender, &channels, &mut state, &mut runtime).await;
        }
        let stop = handle_timed_project_request(
            request,
            timing,
            &actor_sender,
            &channels,
            &mut state,
            &mut runtime,
            residency.as_ref(),
        )
        .await;
        state.sync_runtime(&runtime);
        channels.publish_state(&state);
        if stop {
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

#[allow(clippy::large_futures)]
async fn handle_timed_project_request(
    request: ProjectRequest,
    timing: ProjectRequestTiming,
    actor_sender: &mpsc::WeakSender<ProjectRequest>,
    channels: &ProjectActorChannels,
    state: &mut ProjectState,
    runtime: &mut ProjectRuntime,
    residency: Option<&ProjectResidency>,
) -> bool {
    timing.span.record(
        "actor_queue_ms",
        timing.queued_at.elapsed().as_millis() as u64,
    );
    let started = Instant::now();
    let stop = handle_project_request(request, actor_sender, channels, state, runtime, residency)
        .instrument(timing.span.clone())
        .await;
    timing
        .span
        .record("actor_execution_ms", started.elapsed().as_millis() as u64);
    stop
}

async fn resume_project_runtime(
    actor_sender: &mpsc::WeakSender<ProjectRequest>,
    channels: &ProjectActorChannels,
    state: &mut ProjectState,
    runtime: &mut ProjectRuntime,
) {
    let roots = runtime.translator.workspace_roots().to_vec();
    runtime.begin_transition();
    state.last_error = None;
    channels.publish_status(state, ProjectStatus::Starting);
    let cancellation = CancellationToken::new();
    match Box::pin(run_cancellable_transition(
        &channels.gate,
        cancellation.clone(),
        runtime.activate_workspace_roots(roots, cancellation),
    ))
    .await
    {
        Ok(activation) => {
            mark_project_started(activation, actor_sender, channels, state, runtime);
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
    gate: &ProjectRequestGate,
    generation: u64,
) {
    for (server_id, receiver) in notification_receivers {
        let sender = actor_sender.clone();
        let gate = gate.clone();
        tokio::spawn(forward_lsp_notifications(
            server_id, receiver, sender, gate, generation,
        ));
    }
}

async fn forward_lsp_notifications(
    server_id: ServerId,
    mut receiver: mpsc::Receiver<LspNotification>,
    sender: mpsc::WeakSender<ProjectRequest>,
    gate: ProjectRequestGate,
    generation: u64,
) {
    while let Some(notification) = receiver.recv().await {
        if !gate.is_accepting() {
            break;
        }
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
    if gate.is_accepting()
        && let Some(sender) = sender.upgrade()
    {
        // Closing a receiver is also the normal result of intentional eviction.
        // Let the actor inspect its lifecycle state before acquiring residency.
        let _ = sender
            .send(ProjectRequest::ServerExited { generation })
            .await;
    }
}

fn mark_project_started(
    activation: ProjectActivation,
    actor_sender: &mpsc::WeakSender<ProjectRequest>,
    channels: &ProjectActorChannels,
    state: &mut ProjectState,
    runtime: &mut ProjectRuntime,
) {
    runtime.reset_automatic_restart();
    runtime.record_activation(activation.health());
    spawn_notification_forwarders(
        activation.into_notification_receivers(),
        actor_sender,
        &channels.gate,
        runtime.generation(),
    );
    publish_project_readiness(channels, state, runtime);
}

const fn activation_status(health: ActivationHealth, initializing: bool) -> ProjectStatus {
    if initializing {
        ProjectStatus::Starting
    } else {
        match health {
            ActivationHealth::Ready => ProjectStatus::Ready,
            ActivationHealth::Degraded | ActivationHealth::StructuralOnly => {
                ProjectStatus::Degraded
            }
        }
    }
}

fn publish_project_readiness(
    channels: &ProjectActorChannels,
    state: &mut ProjectState,
    runtime: &ProjectRuntime,
) {
    state.sync_runtime(runtime);
    channels.publish_status(state, runtime.readiness_status());
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
    dormancy: ProjectDormancy,
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
    state.dormancy = Some(dormancy);
    channels.publish_status(state, ProjectStatus::Dormant);
    Ok(())
}

const PROJECT_SHUTDOWN_CANCELLED: &str = "project shutdown requested";

async fn run_cancellable_transition<T, F>(
    gate: &ProjectRequestGate,
    cancellation: CancellationToken,
    operation: F,
) -> Result<T, String>
where
    F: Future<Output = Result<T, String>>,
{
    tokio::pin!(operation);
    if !gate.is_accepting() {
        cancellation.cancel();
        let _ = operation.await;
        return Err(PROJECT_SHUTDOWN_CANCELLED.to_string());
    }

    tokio::select! {
        result = &mut operation => result,
        () = gate.wait_for_rejection() => {
            cancellation.cancel();
            let _ = operation.await;
            Err(PROJECT_SHUTDOWN_CANCELLED.to_string())
        }
    }
}

// This exhaustive dispatcher keeps actor state transitions in one place; each
// request arm is intentionally small and independently typed.
#[allow(clippy::too_many_lines)]
#[allow(clippy::large_stack_frames)]
#[allow(clippy::large_futures)]
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
        ProjectRequest::Timed { .. } => {
            unreachable!("timed request must be unwrapped by the actor loop")
        }
        ProjectRequest::Resident { .. } => {
            unreachable!("resident request must be unwrapped by the actor loop")
        }
        ProjectRequest::Query { reply } | ProjectRequest::Refresh { reply } => {
            state.sync_runtime(runtime);
            let _ = reply.send(state.clone());
        }
        ProjectRequest::Suspend { reply, dormancy } => {
            let _ = reply.send(suspend_project_runtime(channels, state, runtime, dormancy).await);
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
            let cancellation = CancellationToken::new();
            match run_cancellable_transition(
                &channels.gate,
                cancellation.clone(),
                runtime.activate_workspace_roots(vec![root], cancellation),
            )
            .await
            {
                Ok(notification_receivers) => {
                    mark_project_started(
                        notification_receivers,
                        actor_sender,
                        channels,
                        state,
                        runtime,
                    );
                    let _ = reply.send(Ok(state.clone()));
                }
                Err(error) => {
                    state.sync_runtime(runtime);
                    channels.publish_failure(state, error.clone());
                    if let Some(residency) = residency {
                        residency.remove();
                    }
                    let _ = reply.send(Err(error));
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
            let cancellation = CancellationToken::new();
            match run_cancellable_transition(
                &channels.gate,
                cancellation.clone(),
                runtime.activate_workspace_roots(roots, cancellation),
            )
            .await
            {
                Ok(notification_receivers) => {
                    mark_project_started(
                        notification_receivers,
                        actor_sender,
                        channels,
                        state,
                        runtime,
                    );
                    let _ = reply.send(Ok(state.clone()));
                }
                Err(error) => {
                    state.sync_runtime(runtime);
                    channels.publish_failure(state, error.clone());
                    if let Some(residency) = residency {
                        residency.remove();
                    }
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
            limits,
            page_offset,
            reply,
        } => {
            let _ = reply.send(
                runtime
                    .references(
                        file_path,
                        line,
                        character,
                        include_declaration,
                        limits,
                        page_offset,
                    )
                    .await,
            );
        }
        ProjectRequest::ReadSourceResource {
            resource,
            max_response_bytes,
            reply,
        } => {
            let _ = reply.send(
                runtime
                    .read_source_resource(resource, max_response_bytes)
                    .await,
            );
        }
        ProjectRequest::ResolveSymbolHandle {
            symbol_handle,
            reply,
        } => {
            let _ = reply.send(runtime.resolve_symbol_handle(symbol_handle).await);
        }
        ProjectRequest::Diagnostics {
            file_path,
            options,
            reply,
        } => {
            let _ = reply.send(runtime.diagnostics(file_path, options).await);
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
        ProjectRequest::DocumentSymbols { request, reply } => {
            let _ = reply.send(runtime.document_symbols(request).await);
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
        ProjectRequest::GeneratedEditPreview {
            project_id,
            request,
            encoding,
            root,
            reply,
        } => {
            let _ = reply.send(
                runtime
                    .preview_generated_edit(&project_id, request, encoding, &root)
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
        ProjectRequest::WorkspaceSymbol { request, mut reply } => {
            if reply.is_closed() {
                return false;
            }
            let result = tokio::select! {
                () = reply.closed() => return false,
                result = runtime.workspace_symbol_page(request) => result,
            };
            let _ = reply.send(result);
        }
        ProjectRequest::WorkspaceSymbolBatch { request, mut reply } => {
            if reply.is_closed() {
                return false;
            }
            let result = tokio::select! {
                () = reply.closed() => return false,
                result = runtime.workspace_symbol_batch(request) => result,
            };
            let _ = reply.send(result);
        }
        ProjectRequest::LexicalSearch { request, mut reply } => {
            if reply.is_closed() {
                return false;
            }
            let result = tokio::select! {
                () = reply.closed() => return false,
                result = runtime.lexical_search(request) => result,
            };
            let _ = reply.send(result);
        }
        ProjectRequest::LexicalSearchBatch { request, mut reply } => {
            if reply.is_closed() {
                return false;
            }
            let result = tokio::select! {
                () = reply.closed() => return false,
                result = runtime.lexical_search_batch(request) => result,
            };
            let _ = reply.send(result);
        }
        ProjectRequest::InspectSymbol { request, mut reply } => {
            if reply.is_closed() {
                return false;
            }
            let result = tokio::select! {
                () = reply.closed() => return false,
                result = runtime.inspect_symbol(request) => result,
            };
            let _ = reply.send(result);
        }
        ProjectRequest::InspectSymbolBatch { request, mut reply } => {
            if reply.is_closed() {
                return false;
            }
            let result = tokio::select! {
                () = reply.closed() => return false,
                result = runtime.inspect_symbol_batch(*request) => result,
            };
            let _ = reply.send(result);
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
            page_token,
            reply,
        } => {
            let _ = reply.send(
                runtime
                    .prepare_call_hierarchy(file_path, line, character, page_token)
                    .await,
            );
        }
        ProjectRequest::IncomingCalls {
            item,
            limits,
            reply,
        } => {
            let _ = reply.send(runtime.incoming_calls(item, limits).await);
        }
        ProjectRequest::OutgoingCalls {
            item,
            limits,
            reply,
        } => {
            let _ = reply.send(runtime.outgoing_calls(item, limits).await);
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
        ProjectRequest::CachedDiagnostics {
            file_path,
            options,
            reply,
        } => {
            let _ = reply.send(runtime.cached_diagnostics(&file_path, options).await);
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
        ProjectRequest::SourcePathAuthorized { path, reply } => {
            let _ = reply.send(runtime.source_path_is_authorized(&path));
        }
        ProjectRequest::AddWorkspaceRoot { root, reply } => {
            let previous_status = state.status;
            runtime.begin_transition();
            state.last_error = None;
            channels.publish_status(state, ProjectStatus::Restarting);
            let cancellation = CancellationToken::new();
            match run_cancellable_transition(
                &channels.gate,
                cancellation.clone(),
                runtime.add_workspace_root(root, previous_status, cancellation),
            )
            .await
            {
                Ok(notification_receivers) => {
                    mark_project_started(
                        notification_receivers,
                        actor_sender,
                        channels,
                        state,
                        runtime,
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
            let _ = reply.send(
                runtime
                    .preview_edit(&project_id, edit, encoding, &root)
                    .await,
            );
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
        ProjectRequest::InspectEditPlan {
            plan_id,
            project_id,
            reply,
        } => {
            let _ = reply.send(runtime.inspect_edit_plan(&plan_id, &project_id));
        }
        ProjectRequest::ReadEditPlanDiff {
            plan_id,
            project_id,
            reply,
        } => {
            let _ = reply.send(runtime.read_edit_plan_diff(&plan_id, &project_id));
        }
        ProjectRequest::ReadAppliedEditDetail {
            plan_id,
            project_id,
            reply,
        } => {
            let _ = reply.send(runtime.read_applied_edit_detail(&plan_id, &project_id));
        }
        ProjectRequest::ApplyEditPlan {
            plan_id,
            project_id,
            root,
            session_id,
            principal,
            lease,
            reply,
        } => {
            match runtime.prepare_edit_plan_with_context(
                &plan_id,
                &project_id,
                &root,
                session_id,
                principal,
                lease,
            ) {
                Ok(PreparedEditResult::AlreadyApplied(applied)) => {
                    let _ = reply.send(Ok(ApplyEditPlanOutcome::Applied(applied)));
                }
                Ok(PreparedEditResult::AlreadyConflicted(conflict)) => {
                    let _ = reply.send(Ok(ApplyEditPlanOutcome::Conflict(conflict)));
                }
                Ok(PreparedEditResult::Ready(prepared)) => {
                    runtime.active_edit_workers = runtime.active_edit_workers.saturating_add(1);
                    let sender = actor_sender.clone();
                    tokio::spawn(async move {
                        let worker = tokio::task::spawn_blocking(move || {
                            let apply_result = prepared.backup_policy.as_ref().map_or_else(
                                || {
                                    apply_plan_with_documents(
                                        &prepared.boundary,
                                        &prepared.plan,
                                        &prepared.documents,
                                    )
                                },
                                |policy| {
                                    apply_plan_with_documents_and_backup(
                                        &prepared.boundary,
                                        &prepared.plan,
                                        &prepared.documents,
                                        policy,
                                    )
                                },
                            );
                            (prepared, apply_result)
                        })
                        .await;
                        match worker {
                            Ok((prepared, result)) => {
                                let Some(sender) = sender.upgrade() else {
                                    return;
                                };
                                let _ = sender
                                    .send(ProjectRequest::FinalizeEditPlan {
                                        prepared,
                                        result,
                                        reply,
                                    })
                                    .await;
                            }
                            Err(error) => {
                                let _ =
                                    reply.send(Err(format!("edit commit worker failed: {error}")));
                            }
                        }
                    });
                }
                Err(error) => {
                    let _ = reply.send(Err(error));
                }
            }
        }
        ProjectRequest::FinalizeEditPlan {
            prepared,
            result,
            reply,
        } => {
            runtime.active_edit_workers = runtime.active_edit_workers.saturating_sub(1);
            let result = runtime.finish_prepared_edit(*prepared, result).await;
            if let Ok(ApplyEditPlanOutcome::Applied(applied)) = &result {
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
                publish_project_readiness(channels, state, runtime);
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
            let cancellation = CancellationToken::new();
            match run_cancellable_transition(
                &channels.gate,
                cancellation.clone(),
                runtime.restart(cancellation),
            )
            .await
            {
                Ok(notification_receivers) => {
                    mark_project_started(
                        notification_receivers,
                        actor_sender,
                        channels,
                        state,
                        runtime,
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

struct CargoFeatureActorSnapshot {
    mutation: MutationGate,
    roots: Vec<CanonicalRoot>,
    translator_template: Option<std::sync::Arc<TranslatorTemplate>>,
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

type EditInFlight = std::sync::Arc<
    Mutex<HashMap<(String, String), watch::Sender<Option<Result<ApplyEditPlanOutcome, String>>>>>,
>;

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
    deferred_results: std::sync::Arc<std::sync::Mutex<DeferredResultStore>>,
    edit_coordinator: std::sync::Arc<EditCoordinator>,
    edit_in_flight: EditInFlight,
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
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
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

async fn add_actor_roots(
    actor: &ProjectHandle,
    roots: &[CanonicalRoot],
) -> Result<(), ProjectRegistryError> {
    for root in roots.iter().skip(1) {
        actor
            .add_workspace_root(root.as_path().to_path_buf())
            .await
            .map_err(ProjectRegistryError::from)?;
    }
    Ok(())
}

async fn shutdown_project_actors(actors: &[ProjectActorEntry]) {
    for actor in actors {
        let _ = actor.actor.shutdown().await;
    }
}

impl ProjectRegistry {
    fn spawn_actor(
        &self,
        project_id: &ProjectId,
        root: &CanonicalRoot,
        translator_template: Option<&TranslatorTemplate>,
    ) -> ProjectHandle {
        let Some(template) = translator_template else {
            let mut translator = Translator::new();
            translator.set_workspace_roots(vec![root.as_path().to_path_buf()]);
            return spawn_project_actor_with_deferred_results_scoped(
                self.actor_capacity,
                translator,
                None,
                None,
                self.deferred_results.clone(),
                Some(project_id.to_string()),
            );
        };
        let residency = template
            .language_applies_to_root("rust", root.as_path())
            .then(|| ProjectResidency {
                controller: self.rust_residency.clone(),
                group: RustGroupId(self.next_rust_group_id.fetch_add(1, Ordering::Relaxed)),
            });
        spawn_project_actor_with_deferred_results_scoped(
            self.actor_capacity,
            template.translator_for_root(root.as_path().to_path_buf()),
            template.edit_safety().cloned(),
            residency,
            self.deferred_results.clone(),
            Some(project_id.to_string()),
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
            rust_residency: RustResidencyController::new(DEFAULT_RUST_RESIDENCY_LIMIT),
            next_rust_group_id: std::sync::Arc::new(AtomicU64::new(1)),
            deferred_results: std::sync::Arc::new(
                std::sync::Mutex::new(DeferredResultStore::new()),
            ),
            edit_coordinator: std::sync::Arc::new(EditCoordinator::new()),
            edit_in_flight: std::sync::Arc::new(Mutex::new(HashMap::new())),
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

    #[cfg(test)]
    pub(crate) fn with_rust_residency_idle_timeout(mut self, timeout: Duration) -> Self {
        self.rust_residency = RustResidencyController::with_idle_timeout(1, timeout);
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
            for actor in self.actors_for_project(&id).await? {
                actor
                    .set_status(ProjectStatus::Dormant)
                    .await
                    .map_err(ProjectRegistryError::from)?;
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
            let actor = self.spawn_actor(
                identity.id(),
                identity.root(),
                translator_template.as_deref(),
            );
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
        let actor = self.spawn_actor(&project_id, &primary_root, translator_template.as_deref());
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

    /// Read a project's last published actor state without waiting behind actor work.
    ///
    /// # Errors
    ///
    /// Returns an error when the project is not registered or its actor is unavailable.
    pub async fn status(&self, id: &ProjectId) -> Result<ProjectState, ProjectRegistryError> {
        let (_, actors) = self.actor_entries(id).await?;
        Ok(ProjectState::aggregate(
            actors.into_iter().map(|(actor, _)| actor.state_snapshot()),
        ))
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

    /// Return the effective Cargo feature profile for a registered project.
    ///
    /// # Errors
    ///
    /// Returns [`ProjectRegistryError::ProjectNotFound`] when the project is
    /// not registered.
    pub async fn cargo_features(
        &self,
        id: &ProjectId,
    ) -> Result<Option<crate::config::CargoFeatureProfile>, ProjectRegistryError> {
        self.projects
            .read()
            .await
            .get(id)
            .map(|project| {
                project
                    .config
                    .as_ref()
                    .and_then(|config| config.cargo_features.clone())
                    .map(|profile| profile.normalized())
            })
            .ok_or_else(|| ProjectRegistryError::ProjectNotFound(id.clone()))
    }

    /// Replace a project's Rust Cargo feature profile and its actor runtimes.
    ///
    /// New actors are fully activated before they replace the old actors. A
    /// failed activation or persistence write leaves the existing project
    /// untouched.
    ///
    /// # Errors
    ///
    /// Returns an actor, persistence, or project-not-found error when the
    /// replacement cannot be completed.
    pub async fn update_cargo_features(
        &self,
        id: &ProjectId,
        profile: crate::config::CargoFeatureProfile,
    ) -> Result<ProjectState, ProjectRegistryError> {
        let profile = profile.normalized();
        let (old_config, snapshots) = self.cargo_feature_snapshot(id).await?;
        let _mutations = self
            .lock_mutation_gates(
                snapshots
                    .iter()
                    .map(|snapshot| snapshot.mutation.clone())
                    .collect(),
            )
            .await;

        let mut config = old_config.unwrap_or_default();
        config.cargo_features = Some(profile);
        let replacements = self
            .build_cargo_feature_replacements(id, &config, snapshots)
            .await?;
        self.replace_project_actors_transactionally(id, replacements, config)
            .await?;
        self.status(id).await
    }

    async fn replace_project_actors_transactionally(
        &self,
        id: &ProjectId,
        replacements: Vec<ProjectActorEntry>,
        config: ProjectConfig,
    ) -> Result<(), ProjectRegistryError> {
        let Some(old_entry) = self.swap_project_actors(id, replacements, config).await? else {
            return Err(ProjectRegistryError::ProjectNotFound(id.clone()));
        };
        if let Err(error) = self.persist().await {
            let replacements = self.rollback_project_actors(id, old_entry).await;
            shutdown_project_actors(&replacements).await;
            return Err(error);
        }

        self.deferred_results
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .invalidate_scope(id.as_str());
        shutdown_project_actors(&old_entry.actors).await;
        Ok(())
    }

    async fn cargo_feature_snapshot(
        &self,
        id: &ProjectId,
    ) -> Result<(Option<ProjectConfig>, Vec<CargoFeatureActorSnapshot>), ProjectRegistryError> {
        let projects = self.projects.read().await;
        let project = projects
            .get(id)
            .ok_or_else(|| ProjectRegistryError::ProjectNotFound(id.clone()))?;
        let snapshot = (
            project.config.clone(),
            project
                .actors
                .iter()
                .map(|entry| CargoFeatureActorSnapshot {
                    mutation: entry.mutation.clone(),
                    roots: entry.roots.clone(),
                    translator_template: entry.translator_template.clone(),
                })
                .collect(),
        );
        drop(projects);
        Ok(snapshot)
    }

    async fn build_cargo_feature_replacements(
        &self,
        id: &ProjectId,
        config: &ProjectConfig,
        snapshots: Vec<CargoFeatureActorSnapshot>,
    ) -> Result<Vec<ProjectActorEntry>, ProjectRegistryError> {
        let mut replacements = Vec::with_capacity(snapshots.len());
        for snapshot in snapshots {
            let Some(first_root) = snapshot.roots.first() else {
                shutdown_project_actors(&replacements).await;
                return Err(ProjectRegistryError::Actor(ProjectActorError::Operation(
                    "project actor has no workspace roots".to_owned(),
                )));
            };
            let template = snapshot
                .translator_template
                .as_deref()
                .cloned()
                .or_else(|| self.translator_template.as_deref().cloned())
                .unwrap_or_default()
                .with_project_config(config);
            let actor = self.spawn_actor(id, first_root, Some(&template));
            if let Err(error) = add_actor_roots(&actor, &snapshot.roots).await {
                let _ = actor.shutdown().await;
                shutdown_project_actors(&replacements).await;
                return Err(error);
            }
            let roots = snapshot
                .roots
                .iter()
                .map(|root| root.as_path().to_path_buf())
                .collect::<Vec<_>>();
            if let Err(error) = actor.activate_workspace_roots(roots).await {
                let _ = actor.shutdown().await;
                shutdown_project_actors(&replacements).await;
                return Err(error.into());
            }
            let compatibility =
                rust_project_compatibility_key(first_root.as_path(), Some(&template)).await;
            replacements.push(ProjectActorEntry {
                actor,
                mutation: snapshot.mutation,
                compatibility: ProjectCompatibility::Resolved(compatibility),
                translator_template: Some(std::sync::Arc::new(template)),
                roots: snapshot.roots,
            });
        }
        Ok(replacements)
    }

    async fn swap_project_actors(
        &self,
        id: &ProjectId,
        replacements: Vec<ProjectActorEntry>,
        config: ProjectConfig,
    ) -> Result<Option<ProjectEntry>, ProjectRegistryError> {
        let mut projects = self.projects.write().await;
        let Some(project) = projects.get_mut(id) else {
            drop(projects);
            shutdown_project_actors(&replacements).await;
            return Ok(None);
        };
        Ok(Some(ProjectEntry {
            identity: project.identity.clone(),
            actors: std::mem::replace(&mut project.actors, replacements),
            config: project.config.replace(config),
        }))
    }

    async fn rollback_project_actors(
        &self,
        id: &ProjectId,
        old_entry: ProjectEntry,
    ) -> Vec<ProjectActorEntry> {
        let mut projects = self.projects.write().await;
        let Some(project) = projects.get_mut(id) else {
            drop(projects);
            shutdown_project_actors(&old_entry.actors).await;
            return Vec::new();
        };
        let replacements = std::mem::replace(&mut project.actors, old_entry.actors);
        project.config = old_entry.config;
        replacements
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
    ) -> Result<ApplyEditPlanOutcome, ProjectRegistryError> {
        self.apply_edit_plan_with_context(id, plan_id, None, None)
            .await
    }

    #[cfg(test)]
    pub(crate) fn acquire_test_edit_lease(
        &self,
        path: std::path::PathBuf,
    ) -> crate::edit_coordinator::EditLease {
        self.edit_coordinator
            .try_acquire(
                "test-session",
                [crate::edit_coordinator::EditResource::exact(path)],
            )
            .unwrap_or_else(|error| panic!("test edit lease must be available: {error}"))
    }

    /// Inspect a project-owned edit plan without consuming it.
    pub(crate) async fn inspect_edit_plan(
        &self,
        id: &ProjectId,
        plan_id: PlanId,
    ) -> Result<crate::edit_plan::EditPlanApprovalSummary, ProjectRegistryError> {
        self.actor(id)
            .await?
            .inspect_edit_plan(plan_id, id.as_str().to_string())
            .await
            .map_err(ProjectRegistryError::from)
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
    ) -> Result<ApplyEditPlanOutcome, ProjectRegistryError> {
        self.apply_edit_plan_with_wait(
            id,
            plan_id,
            session_id,
            principal,
            Duration::from_millis(250),
        )
        .await
    }

    /// Apply a plan with a bounded path-admission wait.
    ///
    /// # Errors
    ///
    /// Returns a registry or actor error for invalid projects, ownership, or
    /// filesystem failures. Expected edit contention is returned as a
    /// successful [`ApplyEditPlanOutcome::NotReady`] value.
    pub async fn apply_edit_plan_with_wait(
        &self,
        id: &ProjectId,
        plan_id: PlanId,
        session_id: Option<String>,
        principal: Option<String>,
        wait: Duration,
    ) -> Result<ApplyEditPlanOutcome, ProjectRegistryError> {
        let wait = wait.min(MAX_EDIT_ADMISSION_WAIT);
        let key = (id.as_str().to_owned(), plan_id.as_str().to_owned());
        let (leader, receiver) = {
            let mut in_flight = self.edit_in_flight.lock().await;
            let existing = in_flight.get(&key).map(watch::Sender::subscribe);
            let result = existing.map_or_else(
                || {
                    let (sender, receiver) = watch::channel(None);
                    in_flight.insert(key.clone(), sender);
                    (true, receiver)
                },
                |receiver| (false, receiver),
            );
            drop(in_flight);
            result
        };

        if !leader {
            return self.wait_for_in_flight(plan_id, receiver, wait).await;
        }

        let result = self
            .apply_edit_plan_leader(id, plan_id.clone(), session_id, principal, wait)
            .await;
        let shared = result
            .as_ref()
            .map(Clone::clone)
            .map_err(ToString::to_string);
        let sender = self.edit_in_flight.lock().await.remove(&key);
        if let Some(sender) = sender {
            let _ = sender.send(Some(shared));
        }
        result
    }

    async fn wait_for_in_flight(
        &self,
        plan_id: PlanId,
        mut receiver: watch::Receiver<Option<Result<ApplyEditPlanOutcome, String>>>,
        wait: Duration,
    ) -> Result<ApplyEditPlanOutcome, ProjectRegistryError> {
        let completion = tokio::time::timeout(wait, async {
            loop {
                let current = receiver.borrow().clone();
                if let Some(result) = current {
                    return result;
                }
                receiver
                    .changed()
                    .await
                    .map_err(|_| "edit apply coordinator was removed".to_owned())?;
            }
        })
        .await;
        match completion {
            Ok(Ok(outcome)) => Ok(outcome),
            Ok(Err(error)) => Err(ProjectRegistryError::Actor(ProjectActorError::Operation(
                error,
            ))),
            Err(_) => Ok(ApplyEditPlanOutcome::NotReady(EditNotReady {
                plan_id,
                blocked_paths: Vec::new(),
                retry_after_ms: 100,
            })),
        }
    }

    async fn apply_edit_plan_leader(
        &self,
        id: &ProjectId,
        plan_id: PlanId,
        session_id: Option<String>,
        principal: Option<String>,
        wait: Duration,
    ) -> Result<ApplyEditPlanOutcome, ProjectRegistryError> {
        let (identity, actor, _mutation) = self.entry(id).await?;
        let summary = match actor
            .inspect_edit_plan(plan_id.clone(), id.as_str().to_string())
            .await
        {
            Ok(summary) => summary,
            Err(error) if error.to_string().contains("edit plan not found") => {
                // A committed plan is retained as a receipt by the actor. It
                // has no live snapshot to reserve, so let the actor resolve
                // the receipt (or return the original not-found error) rather
                // than turning an idempotent retry into a protocol failure.
                let lease = self
                    .edit_coordinator
                    .try_acquire(plan_id.as_str(), Vec::new())
                    .map_err(|contention| {
                        ProjectRegistryError::Actor(ProjectActorError::Operation(format!(
                            "edit plan is busy: {contention:?}"
                        )))
                    })?;
                return actor
                    .apply_edit_plan_with_lease(
                        plan_id,
                        id.as_str().to_string(),
                        identity.root().as_path().to_path_buf(),
                        session_id,
                        principal,
                        lease,
                    )
                    .await
                    .map_err(ProjectRegistryError::from);
            }
            Err(error) => return Err(ProjectRegistryError::from(error)),
        };
        let resources = summary.coordination_resources();
        let lease = match self
            .edit_coordinator
            .acquire_for(plan_id.as_str(), resources, wait)
            .await
        {
            Ok(lease) => lease,
            Err(contention) => {
                return Ok(ApplyEditPlanOutcome::NotReady(EditNotReady {
                    plan_id,
                    blocked_paths: contention.paths().to_vec(),
                    retry_after_ms: 100,
                }));
            }
        };
        actor
            .apply_edit_plan_with_lease(
                plan_id,
                id.as_str().to_string(),
                identity.root().as_path().to_path_buf(),
                session_id,
                principal,
                lease,
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

    /// Generate an LSP edit and retain its snapshot atomically in the actor.
    pub(crate) async fn preview_generated_edit(
        &self,
        id: &ProjectId,
        request: GeneratedEditRequest,
        encoding: PositionEncoding,
    ) -> Result<GeneratedEditPreview, ProjectRegistryError> {
        let (identity, actor, mutation) = self.entry(id).await?;
        let _mutation = mutation.lock().await;
        actor
            .preview_generated_edit(
                id.as_str().to_string(),
                request,
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

    /// Resolve a path and wake its registered project when it is dormant.
    pub async fn active_actor_for_path(
        &self,
        path: impl AsRef<Path>,
    ) -> Result<ProjectHandle, ProjectRegistryError> {
        let (project_id, actor) = self.project_for_path(path).await?;
        if actor.query().await?.status() == ProjectStatus::Dormant {
            self.activate(&project_id).await?;
        }
        Ok(actor)
    }

    /// Resolve a dependency source previously surfaced by an active LSP.
    pub async fn actor_for_source_path(
        &self,
        path: impl AsRef<Path>,
    ) -> Result<ProjectHandle, ProjectRegistryError> {
        let path = canonicalize(path.as_ref())?;
        let actors = self
            .projects
            .read()
            .await
            .values()
            .flat_map(|project| project.actors.iter().map(|entry| entry.actor.clone()))
            .collect::<Vec<_>>();
        for actor in actors {
            if actor
                .source_path_is_authorized(path.clone())
                .await
                .unwrap_or(false)
            {
                return Ok(actor);
            }
        }
        Err(ProjectIdentityError::UnregisteredPath(path).into())
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

    pub(crate) async fn resolve_symbol_handle(
        &self,
        id: &ProjectId,
        handle: SymbolHandle,
    ) -> Result<(ProjectHandle, ResolvedSymbolTarget), String> {
        let actors = self
            .actors_for_project(id)
            .await
            .map_err(|error| error.to_string())?;
        for actor in actors {
            match actor.resolve_symbol_handle(handle.clone()).await {
                Ok(target) => return Ok((actor, target)),
                Err(error) => {
                    let error = error.to_string();
                    if !error.starts_with("invalid_symbol_handle") {
                        return Err(error);
                    }
                }
            }
        }
        Err("invalid_symbol_handle: unknown or forged handle; rerun symbol discovery".to_owned())
    }

    pub(crate) fn read_deferred_resource(
        &self,
        token: &str,
    ) -> Result<DeferredResourcePayload, String> {
        self.deferred_results
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .read(token)
    }

    pub(crate) fn store_deferred_resource(
        &self,
        id: &ProjectId,
        kind: &str,
        value: serde_json::Value,
    ) -> Result<DeferredResourceReference, String> {
        let encoded = serde_json::to_vec(&value)
            .map_err(|error| format!("failed to encode deferred {kind}: {error}"))?;
        let snapshot_hash = format!("{:x}", Sha256::digest(&encoded));
        Ok(self
            .deferred_results
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert_scoped_kind(value, snapshot_hash, id.as_str(), kind))
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

    pub(crate) async fn actor(
        &self,
        id: &ProjectId,
    ) -> Result<ProjectHandle, ProjectRegistryError> {
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
    use std::sync::Arc;

    use tempfile::TempDir;
    use tokio::io::BufReader;
    use tokio::sync::Mutex as TokioMutex;

    use super::*;

    #[tokio::test]
    async fn lexical_search_skips_non_utf8_files() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("binary.dat"), [0xff]).unwrap();
        fs::write(
            root.path().join("source.rs"),
            "fn status_chip() {}\nfn status_chip_again() {}\n",
        )
        .unwrap();
        let mut translator = Translator::new();
        translator.set_workspace_roots(vec![root.path().to_path_buf()]);
        let runtime = ProjectRuntime::new(translator);

        let scan = runtime
            .lexical_search(LexicalSearchRequest {
                query: "status_chip".to_owned(),
                mode: crate::bridge::LexicalMatchMode::Literal,
                case: crate::bridge::LexicalCaseMode::Sensitive,
                multiline: false,
                max_files: 10,
                max_matches: 1,
                include_generated: false,
                include_paths: Vec::new(),
                exclude_paths: Vec::new(),
                context_lines: 0,
                page_token: None,
            })
            .await
            .unwrap();

        assert_eq!(scan.matches.len(), 1);
        assert_eq!(scan.total_matches, 2);
        assert_eq!(scan.scanned_files, 2);
        assert_eq!(scan.matches[0].project_relative_path, "source.rs");
    }

    #[tokio::test]
    async fn lexical_search_pages_replay_one_immutable_snapshot() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("a.rs"), "fn marker() {}\n").unwrap();
        fs::write(root.path().join("b.rs"), "fn marker() {}\n").unwrap();
        let mut translator = Translator::new();
        translator.set_workspace_roots(vec![root.path().to_path_buf()]);
        let runtime = ProjectRuntime::new(translator);
        let request = || LexicalSearchRequest {
            query: "marker".to_owned(),
            mode: crate::bridge::LexicalMatchMode::Literal,
            case: crate::bridge::LexicalCaseMode::Sensitive,
            multiline: false,
            max_files: 10,
            max_matches: 1,
            include_generated: false,
            include_paths: Vec::new(),
            exclude_paths: Vec::new(),
            context_lines: 0,
            page_token: None,
        };

        let first = runtime.lexical_search(request()).await.unwrap();
        assert_eq!(first.total_matches, 2);
        assert_eq!(first.matches.len(), 1);
        assert_eq!(first.offset, 0);
        fs::write(root.path().join("a.rs"), "fn changed() {}\n").unwrap();

        let second = runtime
            .lexical_search(LexicalSearchRequest {
                page_token: Some(crate::project::lexical_page_cursor(
                    &first.page_token,
                    first.offset + first.matches.len(),
                )),
                ..request()
            })
            .await
            .unwrap();
        assert_eq!(second.total_matches, 2);
        assert_eq!(second.matches.len(), 1);
        assert_eq!(second.offset, 1);
        assert_eq!(second.snapshot_identity, first.snapshot_identity);
        assert_ne!(second.matches[0].source_uri, first.matches[0].source_uri);
    }

    #[tokio::test]
    async fn lexical_search_batch_shares_snapshot_scan_and_match_budget() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("source.rs"), "marker needle marker\n").unwrap();
        let mut translator = Translator::new();
        translator.set_workspace_roots(vec![root.path().to_path_buf()]);
        let runtime = ProjectRuntime::new(translator);

        let batch = runtime
            .lexical_search_batch(LexicalSearchBatchRequest {
                queries: vec![
                    "marker".to_owned(),
                    "needle".to_owned(),
                    "marker".to_owned(),
                ],
                mode: crate::bridge::LexicalMatchMode::Literal,
                case: crate::bridge::LexicalCaseMode::Sensitive,
                multiline: false,
                max_files: 10,
                max_matches: 2,
                include_generated: false,
                include_paths: Vec::new(),
                exclude_paths: Vec::new(),
                context_lines: 0,
                max_bytes: 16 * 1024,
            })
            .await
            .unwrap();

        assert_eq!(batch.unique_queries, 2);
        assert_eq!(batch.scanned_files, 1);
        assert_eq!(batch.entries.len(), 3);
        assert_eq!(batch.entries[0].result.as_ref().unwrap().returned, 2);
        assert!(batch.entries[1].skipped_by_budget);
        assert_eq!(batch.entries[2].reused_from, Some(0));
    }

    #[tokio::test]
    async fn server_exit_forwarder_does_not_pin_after_intentional_shutdown() {
        let (request_sender, mut request_receiver) = mpsc::channel(1);
        let (notification_sender, notification_receiver) = mpsc::channel(1);
        let gate = ProjectRequestGate::new();

        let forwarder = tokio::spawn(forward_lsp_notifications(
            "rust".into(),
            notification_receiver,
            request_sender.downgrade(),
            gate,
            7,
        ));
        drop(notification_sender);

        let request = tokio::time::timeout(Duration::from_secs(1), request_receiver.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            request,
            ProjectRequest::ServerExited { generation: 7 }
        ));
        forwarder.await.unwrap();
    }

    #[tokio::test]
    async fn server_exit_forwarder_suppresses_exit_after_shutdown_begins() {
        let (request_sender, mut request_receiver) = mpsc::channel(1);
        let (notification_sender, notification_receiver) = mpsc::channel(1);
        let gate = ProjectRequestGate::new();

        let forwarder = tokio::spawn(forward_lsp_notifications(
            "rust".into(),
            notification_receiver,
            request_sender.downgrade(),
            gate.clone(),
            8,
        ));
        gate.reject_new_work();
        drop(notification_sender);

        assert!(
            tokio::time::timeout(Duration::from_millis(50), request_receiver.recv())
                .await
                .is_err()
        );
        forwarder.await.unwrap();
    }

    #[tokio::test]
    async fn deferred_resource_reads_survive_actor_lifecycle_changes() {
        let registry = ProjectRegistry::new(2);
        let reference = registry.deferred_results.lock().unwrap().insert_scoped(
            serde_json::json!({"references": [1, 2]}),
            "snapshot".to_owned(),
            "",
        );

        let token = reference
            .uri
            .strip_prefix("mcpls-deferred:///")
            .unwrap()
            .to_owned();
        let payload = registry.read_deferred_resource(&token).unwrap();
        assert_eq!(payload.value, serde_json::json!({"references": [1, 2]}));
        assert_eq!(payload.snapshot_hash, "snapshot");
    }

    #[test]
    fn deferred_resource_invalidation_is_scoped_to_a_project() {
        let mut store = DeferredResultStore::new();
        let first = store.insert_scoped(
            serde_json::json!({"project": "first"}),
            "snapshot-first".to_owned(),
            "first",
        );
        let second = store.insert_scoped(
            serde_json::json!({"project": "second"}),
            "snapshot-second".to_owned(),
            "second",
        );
        let first_token = first.uri.strip_prefix("mcpls-deferred:///").unwrap();
        let second_token = second.uri.strip_prefix("mcpls-deferred:///").unwrap();

        store.invalidate_scope("first");

        assert!(store.read(first_token).is_err());
        assert_eq!(
            store.read(second_token).unwrap().value,
            serde_json::json!({"project": "second"})
        );
    }
    #[test]
    fn deferred_resource_reads_reject_wrong_scope() {
        let mut store = DeferredResultStore::new();
        let reference = store.insert_scoped(
            serde_json::json!({"project": "first"}),
            "snapshot-first".to_owned(),
            "first",
        );
        let token = reference.uri.strip_prefix("mcpls-deferred:///").unwrap();

        assert!(store.read_scoped(token, "second").is_err());
        assert_eq!(
            store.read_scoped(token, "first").unwrap(),
            serde_json::json!({"project": "first"})
        );
    }
    #[tokio::test]
    async fn call_hierarchy_cursor_pages_preserve_counts_and_identity() {
        let items: Vec<_> = (0..65)
            .map(|index| {
                serde_json::json!({
                    "name": format!("item-{index}"),
                    "kind": 12,
                    "uri": format!("file:///item-{index}.rs"),
                    "range": {
                        "start": {"line": 1, "character": 1},
                        "end": {"line": 1, "character": 4}
                    },
                    "selectionRange": {
                        "start": {"line": 1, "character": 1},
                        "end": {"line": 1, "character": 4}
                    },
                    "path": format!("/item-{index}.rs"),
                    "source": {
                        "status": "deferred",
                        "resource": {
                            "uri": format!("mcpls-source:///item-{index}.rs"),
                            "kind": "source_context",
                            "snapshot_hash": "snapshot"
                        }
                    },
                    "symbol_handle": format!("handle-{index}")
                })
            })
            .collect();
        let deferred_results =
            std::sync::Arc::new(std::sync::Mutex::new(DeferredResultStore::new()));
        let runtime = ProjectRuntime::with_deferred_results_scoped(
            Translator::new(),
            None,
            deferred_results,
            Some("project".to_owned()),
        );
        let reference = runtime.deferred_results.lock().unwrap().insert_scoped(
            serde_json::json!({
                "provider": "standard_lsp",
                "kind": "call_hierarchy",
                "total_items": 65,
                "truncated": false,
                "snapshot_hash": "snapshot",
                "items": items,
            }),
            "snapshot".to_owned(),
            "project",
        );

        let first = runtime
            .prepare_call_hierarchy(String::new(), 0, 0, Some(reference.uri))
            .await
            .unwrap();
        assert_eq!(first.total_items, 65);
        assert_eq!(first.returned_items, 64);
        assert_eq!(first.items.first().unwrap().name, "item-0");
        assert_eq!(first.items.last().unwrap().name, "item-63");
        let next_cursor = first.next_cursor.clone().unwrap();

        let second = runtime
            .prepare_call_hierarchy(String::new(), 0, 0, Some(next_cursor))
            .await
            .unwrap();
        assert_eq!(second.total_items, 65);
        assert_eq!(second.returned_items, 1);
        assert_eq!(second.items[0].name, "item-64");
        assert_eq!(second.items[0].path.as_deref(), Some("/item-64.rs"));
        assert_eq!(
            second.items[0]
                .symbol_handle
                .as_ref()
                .map(ToString::to_string)
                .as_deref(),
            Some("handle-64")
        );
        assert!(matches!(
            second.items[0].source.as_ref(),
            Some(crate::bridge::SourceContext::Deferred { resource })
                if resource.snapshot_hash == "snapshot"
        ));
        assert!(second.next_cursor.is_none());
    }

    #[tokio::test]
    async fn server_exit_recovery_acquires_residency_after_exit_is_authoritative() {
        let controller = RustResidencyController::new(1);
        let (second_sender, mut second_receiver) = mpsc::channel(1);
        controller.register(RustGroupId(2), second_sender.downgrade());
        let actor = spawn_project_actor_with_runtime(
            2,
            Translator::new(),
            None,
            Some(ProjectResidency {
                controller: controller.clone(),
                group: RustGroupId(1),
            }),
        );
        actor.set_status(ProjectStatus::Ready).await.unwrap();
        let mut events = actor.subscribe_events();
        let second_guard = controller.acquire(RustGroupId(2)).await;

        actor
            .sender
            .sender
            .send(ProjectRequest::ServerExited { generation: 0 })
            .await
            .unwrap();
        assert_eq!(
            events.recv().await.unwrap(),
            ProjectEvent::ServerExited { generation: 0 }
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(50), events.recv())
                .await
                .is_err()
        );

        drop(second_guard);
        let Some(ProjectRequest::Suspend { reply, .. }) = second_receiver.recv().await else {
            panic!("expected the pinned resident group to be evicted");
        };
        reply.send(Ok(())).unwrap();
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(1), events.recv())
                .await
                .unwrap()
                .unwrap(),
            ProjectEvent::StatusChanged {
                status: ProjectStatus::Restarting,
                ..
            }
        ));
        actor.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn server_exit_recovery_does_not_wait_behind_eviction_transition() {
        let controller = RustResidencyController::with_idle_timeout(1, Duration::ZERO);
        let (victim_sender, mut victim_receiver) = mpsc::channel(1);
        let (replacement_sender, _replacement_receiver) = mpsc::channel(1);
        controller.register(RustGroupId(1), victim_sender.downgrade());
        controller.register(RustGroupId(2), replacement_sender.downgrade());
        drop(controller.acquire(RustGroupId(1)).await);

        let residency = ProjectResidency {
            controller: controller.clone(),
            group: RustGroupId(1),
        };
        let (actor_sender, _actor_receiver) = mpsc::channel(1);
        let (status_tx, _) = watch::channel(ProjectStatus::Ready);
        let (state_tx, _) = watch::channel(ProjectState::new(
            ProjectStatus::Ready,
            ProjectRuntimeSummary::default(),
        ));
        let (event_tx, _) = broadcast::channel(1);
        let channels = ProjectActorChannels {
            status_tx,
            state_tx,
            event_tx,
            event_history: std::sync::Arc::new(std::sync::Mutex::new(ProjectEventHistory::new(1))),
            gate: ProjectRequestGate::new(),
        };
        let mut state = ProjectState::new(ProjectStatus::Ready, ProjectRuntimeSummary::default());
        let mut runtime = ProjectRuntime::new(Translator::new());

        let replacement = tokio::spawn({
            let controller = controller.clone();
            async move { controller.acquire(RustGroupId(2)).await }
        });
        let Some(ProjectRequest::Suspend { reply, .. }) = victim_receiver.recv().await else {
            panic!("expected eviction to suspend the victim");
        };

        let recovery = Box::pin(tokio::time::timeout(
            Duration::from_secs(1),
            handle_server_exit(
                0,
                &actor_sender.downgrade(),
                &channels,
                &mut state,
                &mut runtime,
                Some(&residency),
            ),
        ))
        .await;
        assert!(recovery.is_ok(), "server-exit recovery deadlocked");

        reply.send(Ok(())).unwrap();
        drop(replacement.await.unwrap());
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
    fn symbol_handle_store_is_bounded_and_rejects_forged_handles() {
        let mut store = SymbolHandleStore {
            entries: HashMap::new(),
            ttl: Duration::from_secs(60),
            max_entries: 1,
        };
        let first = store.insert(StoredSymbolTarget::new(
            PathBuf::from("first.rs"),
            1,
            2,
            SourceSnapshot::Version(1),
        ));
        let second = store.insert(StoredSymbolTarget::new(
            PathBuf::from("second.rs"),
            3,
            4,
            SourceSnapshot::Hash("abc".to_owned()),
        ));

        assert!(store.resolve(&first).is_err());
        assert_eq!(store.resolve(&second).unwrap().line, 3);
        assert!(store.resolve(&SymbolHandle::new()).is_err());
    }

    #[test]
    fn project_actor_replacements_start_without_semantic_handles_or_edit_plans() {
        let mut old_runtime = ProjectRuntime::new(Translator::new());
        let handle = old_runtime
            .symbol_handles
            .lock()
            .unwrap()
            .insert(StoredSymbolTarget::new(
                PathBuf::from("src/lib.rs"),
                1,
                2,
                SourceSnapshot::Version(1),
            ));
        let plan = EditPlan::new(
            "project".to_owned(),
            vec![crate::edit_plan::FileSnapshot::from_contents(
                PathBuf::from("src/lib.rs"),
                crate::edit_plan::SnapshotSource::Disk,
                None,
                "before\n",
                "after\n",
            )],
            vec!["replace".to_owned()],
            true,
            Duration::from_secs(60),
        );
        let plan_id = plan.id().clone();
        old_runtime.edit_plans.insert(plan).unwrap();

        let replacement_runtime = ProjectRuntime::new(Translator::new());
        assert!(
            replacement_runtime
                .symbol_handles
                .lock()
                .unwrap()
                .resolve(&handle)
                .is_err()
        );
        assert!(replacement_runtime.edit_plans.get(&plan_id).is_none());
    }

    #[test]
    fn cancelled_inspect_symbol_request_is_discarded_before_actor_work() {
        let (reply, response) = oneshot::channel();
        drop(response);
        let request = ProjectRequest::InspectSymbol {
            request: InspectSymbolRequest {
                symbol_handle: None,
                query: Some("cancelled".to_owned()),
                kind: None,
                path: None,
                container: None,
                candidate_limit: 10,
                sections: Vec::new(),
                budget: crate::bridge::InspectSymbolBudget::default(),
            },
            reply,
        };

        assert!(request.is_cancelled());
    }

    #[test]
    fn inspect_symbol_batch_resumes_a_dormant_rust_runtime() {
        let (reply, _response) = oneshot::channel();
        let request = ProjectRequest::InspectSymbolBatch {
            request: Box::new(crate::bridge::InspectSymbolBatchRequest {
                targets: vec![crate::bridge::InspectSymbolTarget {
                    symbol_handle: None,
                    query: Some("target".to_owned()),
                    kind: None,
                    path: None,
                    container: None,
                }],
                candidate_limit: 10,
                sections: Vec::new(),
                budget: crate::bridge::InspectSymbolBudget::default(),
                page_token: None,
            }),
            reply,
        };

        assert!(request.resumes_rust_runtime());
    }

    #[tokio::test]
    async fn workspace_symbol_handle_survives_handle_clones_and_rejects_stale_source() {
        let root = TempDir::new().unwrap();
        let source = root.path().join("lib.rs");
        fs::write(&source, "fn handle_target() {}\n").unwrap();
        let mut translator = Translator::new()
            .with_extensions(HashMap::from([("rs".to_owned(), "rust".to_owned())]));
        translator.set_workspace_roots(vec![root.path().to_path_buf()]);
        let actor = spawn_project_actor_with_translator(4, translator);

        let result = actor
            .workspace_symbol(WorkspaceSymbolPageRequest {
                query: "handle_target".to_owned(),
                kind_filter: None,
                match_mode: WorkspaceSymbolMatchMode::default(),
                scope: WorkspaceSymbolScope::default(),
                include_generated: false,
                max_items: 10,
                max_bytes: 16 * 1024,
                page_token: None,
            })
            .await
            .unwrap();
        let Some(handle) = result.symbols[0].location.symbol_handle.clone() else {
            panic!("workspace symbol should carry a handle");
        };
        let target = actor
            .clone()
            .resolve_symbol_handle(handle.clone())
            .await
            .unwrap();
        assert_eq!(target.file_path, source.display().to_string());
        assert_eq!(target.character, 4);
        let other_actor = spawn_project_actor_with_translator(4, Translator::new());
        let isolation_error = other_actor
            .resolve_symbol_handle(handle.clone())
            .await
            .unwrap_err();
        assert!(isolation_error.to_string().contains("forged"));

        fs::write(&source, "fn moved_target() {}\n").unwrap();
        let error = actor.resolve_symbol_handle(handle).await.unwrap_err();
        assert!(error.to_string().contains("stale_symbol_handle"));
    }

    #[tokio::test]
    async fn deferred_workspace_symbol_handle_targets_the_identifier() {
        let root = TempDir::new().unwrap();
        let source = root.path().join("lib.rs");
        fs::write(&source, "pub fn add(a: i32, b: i32) -> i32 { a + b }\n").unwrap();
        let mut translator = Translator::new()
            .with_extensions(HashMap::from([("rs".to_owned(), "rust".to_owned())]));
        translator.set_workspace_roots(vec![root.path().to_path_buf()]);
        let (_, _, content_hash, _) = translator.source_snapshot(&source).await.unwrap();
        let runtime = ProjectRuntime::new(translator);
        let mut symbol = WorkspaceSymbol {
            name: "add".to_owned(),
            kind: "Function".to_owned(),
            location: crate::bridge::Location {
                path: Some(source.display().to_string()),
                uri: crate::bridge::path_to_uri(&source).unwrap().to_string(),
                range: crate::bridge::Range {
                    start: crate::bridge::Position2D {
                        line: 1,
                        character: 1,
                    },
                    end: crate::bridge::Position2D {
                        line: 1,
                        character: 4,
                    },
                },
                source: SourceContext::Deferred {
                    resource: crate::bridge::translator::DeferredResourceReference {
                        uri: "mcpls-source://test".to_owned(),
                        kind: "source_context".to_owned(),
                        snapshot_hash: content_hash,
                        document_version: None,
                        total_bytes: None,
                    },
                },
                symbol_handle: None,
            },
            container_name: None,
            match_class: crate::bridge::translator::WorkspaceSymbolMatch::Exact,
            score: 100,
            project_relative_path: Some("lib.rs".to_owned()),
            origin: crate::bridge::translator::WorkspaceSymbolOrigin::ProjectLocal,
            is_generated: false,
        };

        runtime
            .attach_workspace_symbol_handle(&mut symbol, &mut HashMap::new())
            .await;
        let target = runtime
            .resolve_symbol_target(symbol.location.symbol_handle.as_ref().unwrap())
            .await
            .unwrap();
        assert_eq!((target.line, target.character), (1, 8));
    }

    #[tokio::test]
    async fn workspace_symbol_batch_deduplicates_queries_inside_one_actor_request() {
        use crate::bridge::translator::testing::{
            FakeServer, read_framed_message, translator_with_capabilities, write_response,
        };

        let root = TempDir::new().unwrap();
        let alpha = root.path().join("alpha.rs");
        let beta = root.path().join("beta.rs");
        fs::write(&alpha, "fn alpha() {}\n").unwrap();
        fs::write(&beta, "fn beta() {}\n").unwrap();
        let capabilities = lsp_types::ServerCapabilities {
            workspace_symbol_provider: Some(lsp_types::OneOf::Left(true)),
            ..lsp_types::ServerCapabilities::default()
        };
        let (translator, server) =
            translator_with_capabilities(&root, &ServerId::from("rust"), capabilities);
        let FakeServer {
            _write_half,
            _read_half,
            mut read_half_stdin,
            mut write_stdout,
        } = server;
        let responder_alpha = alpha.clone();
        let responder = tokio::spawn(async move {
            let _processes = (_write_half, _read_half);
            let mut reader = BufReader::new(&mut write_stdout);
            let mut queries = Vec::new();
            while queries.len() < 3 {
                let message = read_framed_message(&mut reader).await;
                let Some(id) = message.get("id") else {
                    continue;
                };
                let query = message["params"]["query"].as_str().unwrap().to_owned();
                let uri = if query == "alpha" {
                    &responder_alpha
                } else {
                    &beta
                };
                write_response(
                    &mut read_half_stdin,
                    id,
                    serde_json::json!([{
                        "name": query,
                        "kind": 12,
                        "location": {
                            "uri": path_to_uri(uri).unwrap(),
                            "range": {
                                "start": {"line": 0, "character": 3},
                                "end": {"line": 0, "character": 8}
                            }
                        }
                    }]),
                )
                .await;
                queries.push(query);
            }
            queries
        });
        let actor = spawn_project_actor_with_translator(4, translator);

        let result = actor
            .workspace_symbol_batch(WorkspaceSymbolBatchRequest {
                queries: vec!["alpha".to_owned(), "alpha".to_owned(), "beta".to_owned()],
                kind_filter: None,
                match_mode: WorkspaceSymbolMatchMode::Exact,
                scope: WorkspaceSymbolScope::Project,
                include_generated: false,
                max_items: 10,
                max_bytes: 16 * 1024,
            })
            .await
            .unwrap();

        assert_eq!((result.unique_queries, result.provider_requests), (2, 2));
        assert_eq!(result.entries.len(), 3);
        assert_eq!(result.entries[1].reused_from, Some(0));
        assert!(result.entries[1].result.is_none());
        assert_eq!(result.returned, 2);
        assert!(!result.truncated);
        assert!(serde_json::to_vec(&result).unwrap().len() <= result.max_bytes);

        let repeated = actor
            .workspace_symbol_batch(WorkspaceSymbolBatchRequest {
                queries: vec!["alpha".to_owned(), "beta".to_owned()],
                kind_filter: None,
                match_mode: WorkspaceSymbolMatchMode::Exact,
                scope: WorkspaceSymbolScope::Project,
                include_generated: false,
                max_items: 10,
                max_bytes: 16 * 1024,
            })
            .await
            .unwrap();
        assert_eq!(repeated.provider_requests, 0);
        assert_eq!(repeated.returned, 2);
        assert!(repeated.cache_hit);
        assert_eq!(repeated.snapshot_identity, result.snapshot_identity);

        fs::write(&alpha, "fn alpha_changed() {}\n").unwrap();
        let refreshed = actor
            .workspace_symbol_batch(WorkspaceSymbolBatchRequest {
                queries: vec!["alpha".to_owned()],
                kind_filter: None,
                match_mode: WorkspaceSymbolMatchMode::Exact,
                scope: WorkspaceSymbolScope::Project,
                include_generated: false,
                max_items: 10,
                max_bytes: 16 * 1024,
            })
            .await
            .unwrap();
        assert_eq!(refreshed.provider_requests, 1);
        assert!(!refreshed.cache_hit);
        assert_ne!(result.snapshot_identity, refreshed.snapshot_identity);
        assert_eq!(responder.await.unwrap(), ["alpha", "beta", "alpha"]);
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn workspace_symbol_search_is_bounded_and_pageable() {
        use crate::bridge::translator::testing::{
            FakeServer, read_framed_message, translator_with_capabilities, write_response,
        };

        let root = TempDir::new().unwrap();
        let source = root.path().join("symbols.rs");
        fs::write(&source, "fn get_symbol() {}\n".repeat(100)).unwrap();
        let capabilities = lsp_types::ServerCapabilities {
            workspace_symbol_provider: Some(lsp_types::OneOf::Left(true)),
            ..lsp_types::ServerCapabilities::default()
        };
        let (translator, server) =
            translator_with_capabilities(&root, &ServerId::from("rust"), capabilities);
        let FakeServer {
            _write_half: write_half,
            _read_half: read_half,
            mut read_half_stdin,
            mut write_stdout,
        } = server;
        let responder_source = source.clone();
        let responder = tokio::spawn(async move {
            let _processes = (write_half, read_half);
            let mut reader = BufReader::new(&mut write_stdout);
            let message = read_framed_message(&mut reader).await;
            let id = message.get("id").unwrap();
            let symbols = (0..100)
                .map(|index| {
                    serde_json::json!({
                        "name": format!("get_symbol_{index:03}"),
                        "kind": 12,
                        "location": {
                            "uri": path_to_uri(&responder_source).unwrap(),
                            "range": {
                                "start": {"line": index, "character": 3},
                                "end": {"line": index, "character": 17}
                            }
                        }
                    })
                })
                .collect::<Vec<_>>();
            write_response(&mut read_half_stdin, id, serde_json::json!(symbols)).await;
        });
        let actor = spawn_project_actor_with_translator(4, translator);

        let mut page_token = None;
        let mut snapshot_identity = None;
        let mut source_resource = None;
        let mut names = Vec::new();
        let mut encoded_bytes = 0;
        loop {
            let result = actor
                .workspace_symbol(WorkspaceSymbolPageRequest {
                    query: "get".to_owned(),
                    kind_filter: None,
                    match_mode: WorkspaceSymbolMatchMode::Fuzzy,
                    scope: WorkspaceSymbolScope::Project,
                    include_generated: false,
                    max_items: 100,
                    max_bytes: 16 * 1024,
                    page_token,
                })
                .await
                .unwrap();
            let encoded = serde_json::to_vec(&result).unwrap();
            encoded_bytes += encoded.len();
            assert!(
                encoded.len() <= 16 * 1024,
                "single workspace-symbol page used {} bytes",
                encoded.len()
            );
            assert_eq!(result.total, 100);
            assert!(
                result.symbols.iter().all(|symbol| matches!(
                    &symbol.location.source,
                    SourceContext::Deferred { .. }
                )),
                "workspace-symbol pages must defer source context"
            );
            if source_resource.is_none() {
                let SourceContext::Deferred { resource } = &result.symbols[0].location.source
                else {
                    unreachable!("source contexts were checked above")
                };
                source_resource = Some(resource.uri.clone());
            }
            if let Some(identity) = &snapshot_identity {
                assert_eq!(result.snapshot_identity.as_ref(), Some(identity));
            } else {
                snapshot_identity = result.snapshot_identity.clone();
            }
            names.extend(result.symbols.into_iter().map(|symbol| symbol.name));
            assert_eq!(result.remaining, 100 - names.len());
            let Some(cursor) = result.next_cursor else {
                assert!(!result.truncated);
                break;
            };
            assert!(result.truncated);
            page_token = Some(cursor);
        }

        assert_eq!(
            names,
            (0..100)
                .map(|index| format!("get_symbol_{index:03}"))
                .collect::<Vec<_>>()
        );
        assert!(
            encoded_bytes < 96 * 1024,
            "workspace-symbol pages used {encoded_bytes} bytes total"
        );
        let source = actor
            .read_source_resource(
                crate::bridge::resources::parse_source_uri(&source_resource.unwrap()).unwrap(),
                16 * 1024,
            )
            .await
            .unwrap();
        assert!(source.text.contains("fn get_symbol()"));
        responder.await.unwrap();
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn document_symbols_default_page_is_bounded() {
        use std::fmt::Write as _;

        use crate::bridge::translator::testing::{
            FakeServer, read_framed_message, translator_with_capabilities, write_response,
        };

        let root = TempDir::new().unwrap();
        let source = root.path().join("outline.rs");
        let content = (0..100).fold(String::new(), |mut content, index| {
            writeln!(content, "pub fn caf\u{e9}_{index:03}() {{}}").unwrap();
            content
        });
        fs::write(&source, content).unwrap();
        let capabilities = lsp_types::ServerCapabilities {
            document_symbol_provider: Some(lsp_types::OneOf::Left(true)),
            ..lsp_types::ServerCapabilities::default()
        };
        let (translator, server) =
            translator_with_capabilities(&root, &ServerId::from("rust"), capabilities);
        let FakeServer {
            _write_half: write_half,
            _read_half: read_half,
            mut read_half_stdin,
            mut write_stdout,
        } = server;
        let responder_source = source.clone();
        let responder = tokio::spawn(async move {
            let _processes = (write_half, read_half);
            let mut reader = BufReader::new(&mut write_stdout);
            let message = loop {
                let message = read_framed_message(&mut reader).await;
                if !message["id"].is_null() {
                    break message;
                }
            };
            let id = message.get("id").unwrap();
            let symbols = (0..100)
                .map(|index| {
                    serde_json::json!({
                        "name": format!("caf\u{e9}_{index:03}"),
                        "kind": 12,
                        "location": {
                            "uri": path_to_uri(&responder_source).unwrap(),
                            "range": {
                                "start": {"line": index, "character": 7},
                                "end": {"line": index, "character": 15}
                            }
                        }
                    })
                })
                .collect::<Vec<_>>();
            write_response(&mut read_half_stdin, id, serde_json::json!(symbols)).await;
        });
        let actor = spawn_project_actor_with_translator(4, translator);

        let mut page_token = None;
        let mut snapshot_identity = None;
        let mut source_resource = None;
        let mut names = Vec::new();
        let mut encoded_bytes = 0;
        loop {
            let result = actor
                .document_symbol_page(DocumentSymbolPageRequest {
                    file_path: source.display().to_string(),
                    options: DocumentSymbolOptions::default(),
                    max_bytes: 16 * 1024,
                    page_token,
                })
                .await
                .unwrap();
            let encoded = serde_json::to_vec(&result).unwrap();
            encoded_bytes += encoded.len();
            assert!(
                encoded.len() <= 16 * 1024,
                "document-symbol page used {} bytes",
                encoded.len()
            );
            assert_eq!(result.total, 100);
            assert!(
                result
                    .symbols
                    .iter()
                    .all(|symbol| symbol.children.is_none() && symbol.source.is_none())
            );
            if source_resource.is_none() {
                source_resource = result
                    .source_resource
                    .as_ref()
                    .map(|resource| resource.uri.clone());
            }
            if let Some(identity) = &snapshot_identity {
                assert_eq!(result.snapshot_identity.as_ref(), Some(identity));
            } else {
                snapshot_identity = result.snapshot_identity.clone();
            }
            names.extend(result.symbols.into_iter().map(|symbol| symbol.name));
            assert_eq!(result.remaining, 100 - names.len());
            let Some(cursor) = result.next_cursor else {
                assert!(!result.truncated);
                break;
            };
            assert!(result.truncated);
            page_token = Some(cursor);
        }

        assert_eq!(
            names,
            (0..100)
                .map(|index| format!("caf\u{e9}_{index:03}"))
                .collect::<Vec<_>>()
        );
        assert!(
            encoded_bytes < 32 * 1024,
            "document-symbol pages used {encoded_bytes} bytes"
        );
        let source_page = actor
            .read_source_resource(
                crate::bridge::resources::parse_source_uri(&source_resource.unwrap()).unwrap(),
                16 * 1024,
            )
            .await
            .unwrap();
        assert!(source_page.text.contains("pub fn caf\u{e9}_000"));
        assert!(source_page.text.contains("pub fn caf\u{e9}_099"));
        responder.await.unwrap();
    }

    #[test]
    fn diagnostic_pages_bound_the_transcript_shape_without_losing_occurrences() {
        let diagnostics = (0..249)
            .map(|line| crate::bridge::Diagnostic {
                range: crate::bridge::Range {
                    start: crate::bridge::Position2D { line, character: 1 },
                    end: crate::bridge::Position2D { line, character: 2 },
                },
                severity: DiagnosticSeverity::Hint,
                message: "uniffi::constructor: internal café 🚗 proc-macro error".to_owned(),
                code: Some("macro-error".to_owned()),
                context: crate::bridge::translator::DiagnosticContext {
                    path: Some("/workspace/src/lib.rs".to_owned()),
                    project_relative_path: Some("src/lib.rs".to_owned()),
                    uri: "file:///workspace/src/lib.rs".to_owned(),
                    source_frame: SourceContext::Deferred {
                        resource: DeferredResourceReference {
                            uri: format!(
                                "mcpls-source:///workspace/src/lib.rs?start_line={line}&snapshot={}",
                                "a".repeat(64)
                            ),
                            kind: "source_context".to_owned(),
                            snapshot_hash: "a".repeat(64),
                            document_version: Some(1),
                            total_bytes: Some(512),
                        },
                    },
                    diagnostic_source: Some("rust-analyzer".to_owned()),
                    ..crate::bridge::translator::DiagnosticContext::default()
                },
            })
            .collect();
        let options = DiagnosticOptions {
            preserve_locations: true,
            item_limit: 20,
            byte_limit: 6_000,
            ..DiagnosticOptions::default()
        };
        let mut result = Translator::finish_diagnostics(diagnostics, options);
        result.snapshot_identity = Some("diagnostics-snapshot".to_owned());
        let mut state = DiagnosticsPageState {
            file_path: "/workspace/src/lib.rs".to_owned(),
            fresh: false,
            result,
        };

        let mut lines = Vec::new();
        let mut group_id = None;
        let mut encoded_bytes = 0;
        loop {
            let (page, continuation) = bounded_diagnostics_page(state, 20, 6_000).unwrap();
            let encoded = serde_json::to_vec(&page).unwrap();
            encoded_bytes += encoded.len();
            assert!(encoded.len() <= 6_000, "page used {} bytes", encoded.len());
            assert_eq!(page.total_diagnostics, 249);
            assert_eq!(page.total_groups, 1);
            assert_eq!(
                page.snapshot_identity.as_deref(),
                Some("diagnostics-snapshot")
            );
            assert_eq!(page.diagnostics.len(), 1);
            let group = &page.diagnostics[0];
            if let Some(group_id) = &group_id {
                assert_eq!(group.context.group_id.as_ref(), Some(group_id));
            } else {
                group_id = group.context.group_id.clone();
            }
            assert_eq!(group.context.occurrence_offset, lines.len());
            lines.extend(
                group
                    .context
                    .occurrences
                    .iter()
                    .map(|occurrence| occurrence.range.start.line),
            );
            assert_eq!(page.returned_diagnostics, group.context.occurrences.len());
            assert_eq!(page.remaining_diagnostics, 249 - lines.len());

            let Some(continuation) = continuation else {
                assert!(page.next_cursor.is_none());
                break;
            };
            assert!(page.next_cursor.is_some());
            state = continuation;
        }

        assert_eq!(lines, (0..249).collect::<Vec<_>>());
        assert!(
            encoded_bytes < 64 * 1024,
            "pages used {encoded_bytes} bytes"
        );
    }

    #[test]
    fn measured_swift_outline_fits_one_default_page() {
        let range = crate::bridge::Range {
            start: crate::bridge::Position2D {
                line: 1,
                character: 1,
            },
            end: crate::bridge::Position2D {
                line: 1,
                character: 2,
            },
        };
        let parent_handle = SymbolHandle::new();
        let symbols = (0..33)
            .map(|index| crate::bridge::Symbol {
                name: format!("RideMapLiveContentView_caf\u{e9}_{index:02}"),
                kind: if index == 0 { "Struct" } else { "Property" }.to_owned(),
                range: range.clone(),
                selection_range: range.clone(),
                symbol_handle: Some(if index == 0 {
                    parent_handle.clone()
                } else {
                    SymbolHandle::new()
                }),
                parent_symbol_handle: (index != 0).then(|| parent_handle.clone()),
                container_name: (index != 0).then(|| "RideMapLiveContentView".to_owned()),
                match_class: None,
                score: None,
                source: None,
                is_private: false,
                is_test: false,
                children: None,
            })
            .collect();
        let state = DocumentSymbolPageState {
            total: 33,
            snapshot_identity: "e0f3f3e91aa47c772291d7106b5000a1d487f071bf0eb8d6f8d495f0246f8c06"
                .to_owned(),
            document_version: Some(2),
            project_relative_path: Some(
                "swift/CutoutMobile/Apps/CutoutApp/RideMapLiveContentView.swift".to_owned(),
            ),
            source_resource: DeferredResourceReference {
                uri: "mcpls-source:///Users/mjc/projects/libcutout/swift/CutoutMobile/Apps/CutoutApp/RideMapLiveContentView.swift?start_line=1&start_character=1&end_line=270&end_character=1&snapshot=e0f3f3e91aa47c772291d7106b5000a1d487f071bf0eb8d6f8d495f0246f8c06&version=2".to_owned(),
                kind: "source_context".to_owned(),
                snapshot_hash:
                    "e0f3f3e91aa47c772291d7106b5000a1d487f071bf0eb8d6f8d495f0246f8c06"
                        .to_owned(),
                document_version: Some(2),
                total_bytes: Some(10_346),
            },
            filters: DocumentSymbolOptions {
                include_private: true,
                limit: 50,
                max_depth: Some(2),
                ..DocumentSymbolOptions::default()
            },
            symbols,
        };

        let (result, continuation) = bounded_document_symbol_page(state, 50, 16 * 1024).unwrap();

        assert!(continuation.is_none());
        assert_eq!(result.returned, 33);
        assert_eq!(result.remaining, 0);
        assert!(!result.truncated);
        assert!(serde_json::to_vec(&result).unwrap().len() <= 16 * 1024);
        assert_eq!(result.symbols[0].symbol_handle, Some(parent_handle.clone()));
        assert!(
            result.symbols[1..]
                .iter()
                .all(|symbol| symbol.parent_symbol_handle.as_ref() == Some(&parent_handle))
        );
        assert!(
            result
                .symbols
                .iter()
                .any(|symbol| symbol.name.contains('é'))
        );
    }

    #[test]
    fn flattened_document_symbols_keep_exact_parent_identity() {
        let range = crate::bridge::Range {
            start: crate::bridge::Position2D {
                line: 1,
                character: 1,
            },
            end: crate::bridge::Position2D {
                line: 1,
                character: 2,
            },
        };
        let symbol = |name: &str, children| crate::bridge::Symbol {
            name: name.to_owned(),
            kind: "Function".to_owned(),
            range: range.clone(),
            selection_range: range.clone(),
            symbol_handle: None,
            parent_symbol_handle: None,
            container_name: None,
            match_class: None,
            score: None,
            source: None,
            is_private: false,
            is_test: false,
            children,
        };
        let mut symbols = vec![symbol("parent", Some(vec![symbol("child", None)]))];
        attach_document_symbol_handles(
            &mut SymbolHandleStore::new(),
            &mut symbols,
            Path::new("/tmp/outline.rs"),
            &SourceSnapshot::Hash("snapshot".to_owned()),
            None,
        );

        let flat = flatten_document_symbols(symbols);

        assert_eq!(flat.len(), 2);
        assert_eq!(flat[1].parent_symbol_handle, flat[0].symbol_handle);
        assert!(flat.iter().all(|symbol| symbol.children.is_none()));
    }

    #[tokio::test]
    async fn workspace_symbol_batches_reuse_143_query_provider_results_across_calls() {
        use crate::bridge::translator::testing::{
            FakeServer, read_framed_message, translator_with_capabilities, write_response,
        };

        let root = TempDir::new().unwrap();
        let source = root.path().join("symbols.rs");
        fs::write(&source, "fn symbol() {}\n").unwrap();
        let capabilities = lsp_types::ServerCapabilities {
            workspace_symbol_provider: Some(lsp_types::OneOf::Left(true)),
            ..lsp_types::ServerCapabilities::default()
        };
        let (translator, server) =
            translator_with_capabilities(&root, &ServerId::from("rust"), capabilities);
        let FakeServer {
            _write_half,
            _read_half,
            mut read_half_stdin,
            mut write_stdout,
        } = server;
        let responder = tokio::spawn(async move {
            let _processes = (_write_half, _read_half);
            let mut reader = BufReader::new(&mut write_stdout);
            let mut queries = Vec::new();
            while queries.len() < 117 {
                let message = read_framed_message(&mut reader).await;
                let Some(id) = message.get("id") else {
                    continue;
                };
                let query = message["params"]["query"].as_str().unwrap().to_owned();
                write_response(
                    &mut read_half_stdin,
                    id,
                    serde_json::json!([{
                        "name": query,
                        "kind": 12,
                        "location": {
                            "uri": path_to_uri(&source).unwrap(),
                            "range": {
                                "start": {"line": 0, "character": 3},
                                "end": {"line": 0, "character": 9}
                            }
                        }
                    }]),
                )
                .await;
                queries.push(query);
            }
            queries
        });
        let actor = spawn_project_actor_with_translator(8, translator);
        let unique = (0..117)
            .map(|index| format!("symbol_{index}"))
            .collect::<Vec<_>>();
        let mut queries = unique.clone();
        queries.extend(unique.iter().take(26).cloned());

        let mut client_calls = 0;
        let mut provider_requests = 0;
        for chunk in queries.chunks(32) {
            let result = actor
                .workspace_symbol_batch(WorkspaceSymbolBatchRequest {
                    queries: chunk.to_vec(),
                    kind_filter: None,
                    match_mode: WorkspaceSymbolMatchMode::Exact,
                    scope: WorkspaceSymbolScope::Project,
                    include_generated: false,
                    max_items: 1_000,
                    max_bytes: 64 * 1024,
                })
                .await
                .unwrap();
            client_calls += 1;
            provider_requests += result.provider_requests;
            assert_eq!(result.entries.len(), chunk.len());
        }

        assert_eq!(client_calls, 5);
        assert_eq!(provider_requests, 117);
        assert_eq!(responder.await.unwrap().len(), 117);
    }

    #[tokio::test]
    async fn inspect_symbol_batch_fetches_targets_concurrently_under_one_global_budget() {
        use crate::bridge::translator::testing::{
            FakeServer, read_framed_message, translator_with_capabilities, write_response,
        };

        let root = TempDir::new().unwrap();
        fs::write(root.path().join("alpha.rs"), "fn alpha() {}\n").unwrap();
        fs::write(root.path().join("beta.rs"), "fn beta() {}\n").unwrap();
        let capabilities = lsp_types::ServerCapabilities {
            hover_provider: Some(lsp_types::HoverProviderCapability::Simple(true)),
            ..lsp_types::ServerCapabilities::default()
        };
        let (translator, server) =
            translator_with_capabilities(&root, &ServerId::from("rust"), capabilities);
        let FakeServer {
            _write_half,
            _read_half,
            mut read_half_stdin,
            mut write_stdout,
        } = server;
        let (release_server, hold_server) = oneshot::channel();
        let responder = tokio::spawn(async move {
            let _processes = (_write_half, _read_half);
            let mut reader = BufReader::new(&mut write_stdout);
            let mut request_ids = Vec::new();
            while request_ids.len() < 2 {
                let message = read_framed_message(&mut reader).await;
                if let Some(id) = message.get("id").cloned() {
                    assert_eq!(message["method"], "textDocument/hover");
                    request_ids.push(id);
                }
            }
            for id in &request_ids {
                write_response(
                    &mut read_half_stdin,
                    id,
                    serde_json::json!({
                        "contents": {"kind": "plaintext", "value": "inspected"}
                    }),
                )
                .await;
            }
            let _ = hold_server.await;
            request_ids.len()
        });
        let actor = spawn_project_actor_with_translator(4, translator);
        let mut targets = Vec::new();
        for query in ["alpha", "beta"] {
            let symbol = actor
                .workspace_symbol(WorkspaceSymbolPageRequest {
                    query: query.to_owned(),
                    kind_filter: None,
                    match_mode: WorkspaceSymbolMatchMode::Exact,
                    scope: WorkspaceSymbolScope::Project,
                    include_generated: false,
                    max_items: 1,
                    max_bytes: 16 * 1024,
                    page_token: None,
                })
                .await
                .unwrap()
                .symbols
                .remove(0);
            targets.push(crate::bridge::InspectSymbolTarget {
                symbol_handle: symbol.location.symbol_handle,
                query: None,
                kind: None,
                path: None,
                container: None,
            });
        }
        targets.push(crate::bridge::InspectSymbolTarget {
            symbol_handle: Some(SymbolHandle::new()),
            query: None,
            kind: None,
            path: None,
            container: None,
        });

        let result = tokio::time::timeout(
            Duration::from_secs(1),
            actor.inspect_symbol_batch(crate::bridge::InspectSymbolBatchRequest {
                targets,
                candidate_limit: 10,
                sections: vec![crate::bridge::InspectSymbolSectionKind::Declaration],
                budget: crate::bridge::InspectSymbolBudget {
                    max_bytes: 24 * 1024,
                    max_items: 3,
                },
                page_token: None,
            }),
        )
        .await
        .expect("batch serialized target inspections")
        .unwrap();

        assert_eq!(result.entries.len(), 3);
        assert_eq!(result.inspections_started, 3);
        assert_eq!(result.total_targets, 3);
        assert_eq!(result.returned_targets, 3);
        assert_eq!(result.remaining_targets, 0);
        assert!(result.next_cursor.is_none());
        assert_eq!(result.budget.max_bytes, 16 * 1024);
        assert_eq!(result.returned_items, 2, "{result:#?}");
        assert!(result.entries[..2].iter().all(|entry| matches!(
            entry.result.as_ref().unwrap().resolution,
            crate::bridge::InspectSymbolResolution::Selected { .. }
        )));
        assert!(
            result.entries[..2]
                .iter()
                .all(|entry| entry.result.as_ref().unwrap().budget.max_items == 1)
        );
        assert!(result.entries[2].result.is_none());
        assert!(
            result.entries[2]
                .error
                .as_deref()
                .is_some_and(|error| { error.starts_with("invalid_symbol_handle:") })
        );
        assert!(result.returned_bytes <= result.budget.max_bytes);
        release_server.send(()).unwrap();
        assert_eq!(responder.await.unwrap(), 2);
    }

    #[test]
    fn inspect_symbol_batch_pages_are_bounded_replayable_and_lossless() {
        let entries = (0..4)
            .map(|index| InspectSymbolBatchEntry {
                target: crate::bridge::InspectSymbolTarget {
                    symbol_handle: None,
                    query: Some(format!("target-{index}")),
                    kind: None,
                    path: None,
                    container: None,
                },
                result: None,
                error: Some("x".repeat(5_000)),
                resource: None,
            })
            .collect::<Vec<_>>();
        let snapshot = InspectSymbolBatchSnapshot {
            inspections_started: entries.len(),
            entries,
            snapshot_identity: "snapshot".to_owned(),
            truncated: false,
            max_items: 40,
        };
        let mut store = InspectSymbolBatchPageStore::new();
        let token = store.insert(snapshot.clone(), "session");
        assert!(store.read(&token, "different-session").is_err());

        let first = bounded_inspect_symbol_batch_page(&snapshot, &token, 0).unwrap();
        let first_json = serde_json::to_value(&first).unwrap();
        assert!(first.next_cursor.is_some());
        let mut cursor = Some(inspect_symbol_batch_cursor(&token, 0));
        let mut queries = Vec::new();
        let mut pages = 0;
        while let Some(page_cursor) = cursor {
            let (page_token, offset) = parse_inspect_symbol_batch_cursor(&page_cursor).unwrap();
            let retained = store.read(page_token, "session").unwrap();
            let page = bounded_inspect_symbol_batch_page(&retained, page_token, offset).unwrap();
            let encoded_len = serde_json::to_vec(&page).unwrap().len();
            assert!(encoded_len <= 16 * 1024);
            assert_eq!(page.returned_bytes, encoded_len);
            queries.extend(
                page.entries
                    .iter()
                    .map(|entry| entry.target.query.clone().unwrap()),
            );
            cursor = page.next_cursor;
            pages += 1;
        }

        assert!(pages > 1);
        assert_eq!(queries, ["target-0", "target-1", "target-2", "target-3"]);
        assert_eq!(
            serde_json::to_value(bounded_inspect_symbol_batch_page(&snapshot, &token, 0).unwrap())
                .unwrap(),
            first_json,
            "replaying a page must return the same cursor and content"
        );
    }

    #[tokio::test]
    async fn inspect_symbol_runs_as_one_actor_request_and_honors_section_selection() {
        let root = TempDir::new().unwrap();
        let source = root.path().join("lib.rs");
        fs::write(&source, "fn inspected() {}\n").unwrap();
        let mut translator = Translator::new()
            .with_extensions(HashMap::from([("rs".to_owned(), "rust".to_owned())]));
        translator.set_workspace_roots(vec![root.path().to_path_buf()]);
        let actor = spawn_project_actor_with_translator(4, translator);
        let mut symbols = actor
            .workspace_symbol(WorkspaceSymbolPageRequest {
                query: "inspected".to_owned(),
                kind_filter: None,
                match_mode: WorkspaceSymbolMatchMode::Exact,
                scope: WorkspaceSymbolScope::Project,
                include_generated: false,
                max_items: 10,
                max_bytes: 16 * 1024,
                page_token: None,
            })
            .await
            .unwrap()
            .symbols;
        let symbol = symbols.remove(0);
        let result = actor
            .inspect_symbol(InspectSymbolRequest {
                symbol_handle: symbol.location.symbol_handle,
                query: None,
                kind: None,
                path: None,
                container: None,
                candidate_limit: 10,
                sections: vec![crate::bridge::InspectSymbolSectionKind::References],
                budget: crate::bridge::InspectSymbolBudget {
                    max_bytes: 4_096,
                    max_items: 3,
                },
            })
            .await
            .unwrap();

        assert!(matches!(
            result.resolution,
            crate::bridge::InspectSymbolResolution::Selected { .. }
        ));
        assert_eq!(
            result.sections.declaration.completeness,
            crate::bridge::InspectSectionCompleteness::NotRequested
        );
        assert_eq!(result.sections.references.returned, 0);
        assert!(result.returned_bytes <= result.budget.max_bytes);
    }

    #[tokio::test]
    async fn inspect_symbol_fetches_independent_sections_concurrently() {
        use crate::bridge::translator::testing::{
            FakeServer, read_framed_message, translator_with_capabilities, write_response,
        };

        let root = TempDir::new().unwrap();
        let source = root.path().join("lib.rs");
        fs::write(&source, "fn inspected() {}\n").unwrap();
        let uri = crate::bridge::path_to_uri(&source).unwrap();
        let server_id = ServerId::from("rust");
        let capabilities = lsp_types::ServerCapabilities {
            hover_provider: Some(lsp_types::HoverProviderCapability::Simple(true)),
            implementation_provider: Some(lsp_types::ImplementationProviderCapability::Simple(
                true,
            )),
            references_provider: Some(lsp_types::OneOf::Left(true)),
            definition_provider: Some(lsp_types::OneOf::Left(true)),
            call_hierarchy_provider: Some(lsp_types::CallHierarchyServerCapability::Simple(true)),
            ..lsp_types::ServerCapabilities::default()
        };
        let (translator, server) = translator_with_capabilities(&root, &server_id, capabilities);
        let FakeServer {
            _write_half,
            _read_half,
            read_half_stdin,
            mut write_stdout,
        } = server;
        let (release_server, hold_server) = oneshot::channel();
        let responder = tokio::spawn(async move {
            let _processes = (_write_half, _read_half);
            let writer = Arc::new(TokioMutex::new(read_half_stdin));
            let mut reader = BufReader::new(&mut write_stdout);
            let hover = loop {
                let message = read_framed_message(&mut reader).await;
                if message.get("id").is_some() {
                    break message;
                }
            };
            assert_eq!(hover["method"], "textDocument/hover");
            assert!(
                tokio::time::timeout(Duration::from_millis(50), read_framed_message(&mut reader))
                    .await
                    .is_err(),
                "sections must wait for the declaration preflight"
            );
            {
                let mut writer = writer.lock().await;
                write_response(
                    &mut *writer,
                    &hover["id"],
                    serde_json::json!({"contents": {"kind": "plaintext", "value": "inspected"}}),
                )
                .await;
            }

            let mut requests = 1;
            let mut responses = Vec::new();
            while requests < 7 {
                let message = read_framed_message(&mut reader).await;
                let Some(id) = message.get("id").cloned() else {
                    continue;
                };
                requests += 1;
                let method = message["method"].as_str().unwrap().to_owned();
                let uri = uri.clone();
                let writer = Arc::clone(&writer);
                responses.push(tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    let result = match method.as_str() {
                        "textDocument/hover" => serde_json::json!({
                            "contents": {"kind": "plaintext", "value": "inspected"}
                        }),
                        "textDocument/prepareCallHierarchy" => serde_json::json!([{
                            "name": "inspected",
                            "kind": 12,
                            "uri": uri,
                            "range": {
                                "start": {"line": 0, "character": 0},
                                "end": {"line": 0, "character": 17}
                            },
                            "selectionRange": {
                                "start": {"line": 0, "character": 3},
                                "end": {"line": 0, "character": 12}
                            }
                        }]),
                        "textDocument/definition" => serde_json::Value::Null,
                        _ => serde_json::json!([]),
                    };
                    let mut writer = writer.lock().await;
                    write_response(&mut *writer, &id, result).await;
                }));
            }
            for response in responses {
                response.await.unwrap();
            }
            let _ = hold_server.await;
        });

        let runtime = ProjectRuntime::new(translator);
        let (_, _, source_hash, _) = runtime.translator.source_snapshot(&source).await.unwrap();
        let handle = runtime
            .symbol_handles
            .lock()
            .unwrap()
            .insert(StoredSymbolTarget::new(
                source,
                1,
                4,
                SourceSnapshot::Hash(source_hash),
            ));
        let started = Instant::now();
        let result = runtime
            .inspect_symbol(InspectSymbolRequest {
                symbol_handle: Some(handle),
                query: None,
                kind: None,
                path: None,
                container: None,
                candidate_limit: 5,
                sections: vec![
                    crate::bridge::InspectSymbolSectionKind::Declaration,
                    crate::bridge::InspectSymbolSectionKind::Implementations,
                    crate::bridge::InspectSymbolSectionKind::References,
                    crate::bridge::InspectSymbolSectionKind::Calls,
                ],
                budget: crate::bridge::InspectSymbolBudget {
                    max_bytes: 30_000,
                    max_items: 80,
                },
            })
            .await
            .unwrap();

        assert!(
            started.elapsed() < Duration::from_millis(450),
            "independent sections ran serially in {:?}",
            started.elapsed()
        );
        assert_eq!(
            result.sections.calls.completeness,
            crate::bridge::InspectSectionCompleteness::Complete,
            "{:?}",
            result.sections.calls.reason
        );
        release_server.send(()).unwrap();
        responder.await.unwrap();
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn inspect_symbol_path_resolution_does_not_request_document_outline() {
        use crate::bridge::translator::testing::{
            FakeServer, read_framed_message, translator_with_capabilities, write_response,
        };

        let root = TempDir::new().unwrap();
        let source = root.path().join("lib.rs");
        let other = root.path().join("other.rs");
        fs::write(&source, "fn inspected() {}\n").unwrap();
        fs::write(&other, "fn inspected() {}\n").unwrap();
        let uri = crate::bridge::path_to_uri(&source).unwrap();
        let other_uri = crate::bridge::path_to_uri(&other).unwrap();
        let server_id = ServerId::from("rust");
        let capabilities = lsp_types::ServerCapabilities {
            document_symbol_provider: Some(lsp_types::OneOf::Left(true)),
            workspace_symbol_provider: Some(lsp_types::OneOf::Left(true)),
            ..lsp_types::ServerCapabilities::default()
        };
        let (translator, server) = translator_with_capabilities(&root, &server_id, capabilities);
        let FakeServer {
            _write_half: write_half,
            _read_half: read_half,
            mut read_half_stdin,
            mut write_stdout,
        } = server;
        let (method_tx, method_rx) = oneshot::channel();
        let (release_server, hold_server) = oneshot::channel();
        let responder = tokio::spawn(async move {
            let processes = (write_half, read_half);
            let mut reader = BufReader::new(&mut write_stdout);
            loop {
                let message = read_framed_message(&mut reader).await;
                let Some(id) = message.get("id") else {
                    continue;
                };
                let method = message["method"].as_str().unwrap().to_owned();
                method_tx.send(method.clone()).unwrap();
                let response = if method == "workspace/symbol" {
                    serde_json::json!([
                        {
                            "name": "inspected",
                            "kind": 12,
                            "location": {
                                "uri": other_uri,
                                "range": {
                                    "start": {"line": 0, "character": 3},
                                    "end": {"line": 0, "character": 12}
                                }
                            }
                        },
                        {
                            "name": "inspected",
                            "kind": 12,
                            "location": {
                                "uri": uri,
                                "range": {
                                    "start": {"line": 0, "character": 3},
                                    "end": {"line": 0, "character": 12}
                                }
                            }
                        }
                    ])
                } else {
                    serde_json::json!([])
                };
                write_response(&mut read_half_stdin, id, response).await;
                break;
            }
            let _ = hold_server.await;
            drop(processes);
        });

        let runtime = ProjectRuntime::new(translator);
        let result = Box::pin(runtime.inspect_symbol(InspectSymbolRequest {
            symbol_handle: None,
            query: Some("inspected".to_owned()),
            kind: None,
            path: Some("lib.rs".to_owned()),
            container: None,
            candidate_limit: 1,
            sections: Vec::new(),
            budget: crate::bridge::InspectSymbolBudget::default(),
        }))
        .await;

        assert_eq!(method_rx.await.unwrap(), "workspace/symbol");
        let result = result.unwrap();
        let crate::bridge::InspectSymbolResolution::Selected {
            symbol: Some(symbol),
            ..
        } = result.resolution
        else {
            panic!("requested path must select its symbol before applying the candidate limit");
        };
        assert_eq!(
            symbol.location.path.as_deref(),
            Some(source.to_str().unwrap())
        );
        release_server.send(()).unwrap();
        responder.await.unwrap();
    }

    #[test]
    fn empty_call_hierarchy_is_not_misclassified_as_non_callable() {
        let section = missing_call_hierarchy_item();

        assert_eq!(
            section.completeness,
            crate::bridge::InspectSectionCompleteness::Unavailable
        );
        assert_eq!(
            section.reason.as_deref(),
            Some("call hierarchy provider returned no item at the symbol selection")
        );
    }

    #[tokio::test]
    async fn inspect_symbol_returns_source_bearing_candidates_for_duplicate_names() {
        let root = TempDir::new().unwrap();
        fs::write(root.path().join("one.rs"), "fn duplicate() -> u8 { 1 }\n").unwrap();
        fs::write(root.path().join("two.rs"), "fn duplicate() -> u8 { 2 }\n").unwrap();
        let mut translator = Translator::new()
            .with_extensions(HashMap::from([("rs".to_owned(), "rust".to_owned())]));
        translator.set_workspace_roots(vec![root.path().to_path_buf()]);
        let actor = spawn_project_actor_with_translator(4, translator);
        let result = actor
            .inspect_symbol(InspectSymbolRequest {
                symbol_handle: None,
                query: Some("duplicate".to_owned()),
                kind: Some("function".to_owned()),
                path: None,
                container: None,
                candidate_limit: 10,
                sections: Vec::new(),
                budget: crate::bridge::InspectSymbolBudget::default(),
            })
            .await
            .unwrap();

        let crate::bridge::InspectSymbolResolution::Ambiguous { candidates } = result.resolution
        else {
            panic!("duplicate symbols must remain ambiguous");
        };
        assert_eq!(candidates.len(), 2);
        assert!(candidates.iter().all(|candidate| matches!(
            candidate.location.source,
            crate::bridge::SourceContext::Available(_)
        )));
    }

    #[tokio::test]
    async fn inspect_symbol_treats_a_large_requested_budget_as_an_upper_bound() {
        const PAGE_LIMIT: usize = 16 * 1024;

        let root = TempDir::new().unwrap();
        for index in 0..40 {
            fs::write(
                root.path().join(format!("duplicate_{index}.rs")),
                format!(
                    "// {}\nfn duplicate() -> u8 {{ {index} }}\n",
                    "x".repeat(1_024)
                ),
            )
            .unwrap();
        }
        let mut translator = Translator::new()
            .with_extensions(HashMap::from([("rs".to_owned(), "rust".to_owned())]));
        translator.set_workspace_roots(vec![root.path().to_path_buf()]);
        let actor = spawn_project_actor_with_translator(4, translator);

        let result = actor
            .inspect_symbol(InspectSymbolRequest {
                symbol_handle: None,
                query: Some("duplicate".to_owned()),
                kind: Some("function".to_owned()),
                path: None,
                container: None,
                candidate_limit: 100,
                sections: Vec::new(),
                budget: crate::bridge::InspectSymbolBudget {
                    max_bytes: 45_000,
                    max_items: 80,
                },
            })
            .await
            .unwrap();

        assert_eq!(result.budget.max_bytes, PAGE_LIMIT);
        let serialized = serde_json::to_vec(&result).unwrap();
        assert!(serialized.len() <= PAGE_LIMIT);
        assert_eq!(result.returned_bytes, serialized.len());
        assert!(result.truncated);
    }

    #[test]
    fn symbol_handle_store_expires_entries() {
        let mut store = SymbolHandleStore {
            entries: HashMap::new(),
            ttl: Duration::ZERO,
            max_entries: 1,
        };
        let handle = store.insert(StoredSymbolTarget::new(
            PathBuf::from("lib.rs"),
            1,
            1,
            SourceSnapshot::Version(1),
        ));
        assert!(store.resolve(&handle).is_err());
    }

    #[tokio::test]
    async fn symbol_handle_rejects_a_new_dirty_document_version() {
        let root = TempDir::new().unwrap();
        let source = root.path().join("dirty.rs");
        fs::write(&source, "fn before() {}\n").unwrap();
        let mut translator = Translator::new();
        translator.set_workspace_roots(vec![root.path().to_path_buf()]);
        translator
            .document_tracker()
            .open(source.clone(), "fn before() {}\n".to_owned())
            .unwrap();
        let runtime = ProjectRuntime::new(translator);
        let handle = runtime
            .symbol_handles
            .lock()
            .unwrap()
            .insert(StoredSymbolTarget::new(
                source.clone(),
                1,
                4,
                SourceSnapshot::Version(1),
            ));
        runtime
            .translator
            .document_tracker()
            .update(&source, "fn after() {}\n".to_owned());

        let error = runtime.resolve_symbol_target(&handle).await.unwrap_err();
        assert!(error.contains("stale_symbol_handle"));
    }

    #[tokio::test]
    async fn inspect_symbol_returns_retryable_result_for_stale_handle() {
        let root = TempDir::new().unwrap();
        let source = root.path().join("stale.rs");
        fs::write(&source, "fn before() {}\n").unwrap();
        let mut translator = Translator::new();
        translator.set_workspace_roots(vec![root.path().to_path_buf()]);
        let runtime = ProjectRuntime::new(translator);
        let (_, _, source_hash, _) = runtime.translator.source_snapshot(&source).await.unwrap();
        let handle = runtime
            .symbol_handles
            .lock()
            .unwrap()
            .insert(StoredSymbolTarget::new(
                source.clone(),
                1,
                4,
                SourceSnapshot::Hash(source_hash),
            ));
        fs::write(&source, "fn after() {}\n").unwrap();

        let result = runtime
            .inspect_symbol(InspectSymbolRequest {
                symbol_handle: Some(handle.clone()),
                query: None,
                kind: None,
                path: None,
                container: None,
                candidate_limit: 5,
                sections: Vec::new(),
                budget: crate::bridge::InspectSymbolBudget::default(),
            })
            .await
            .unwrap();
        let crate::bridge::InspectSymbolResolution::Stale {
            symbol_handle,
            reason,
            retryable,
        } = result.resolution
        else {
            panic!("stale handles must produce a structured refresh result");
        };
        assert_eq!(symbol_handle, handle);
        assert!(retryable);
        assert!(reason.starts_with("stale_symbol_handle:"));
    }

    #[tokio::test]
    async fn inspect_symbol_accepts_a_handle_bound_to_the_current_dirty_version() {
        let root = TempDir::new().unwrap();
        let source = root.path().join("dirty.rs");
        fs::write(&source, "fn disk_name() {}\n").unwrap();
        let mut translator = Translator::new()
            .with_extensions(HashMap::from([("rs".to_owned(), "rust".to_owned())]));
        translator.set_workspace_roots(vec![root.path().to_path_buf()]);
        translator
            .document_tracker()
            .open(source, "fn dirty_name() {}\n".to_owned())
            .unwrap();
        let runtime = ProjectRuntime::new(translator);
        let handle = runtime
            .symbol_handles
            .lock()
            .unwrap()
            .insert(StoredSymbolTarget::new(
                root.path().join("dirty.rs"),
                1,
                4,
                SourceSnapshot::Version(1),
            ));

        let result = Box::pin(runtime.inspect_symbol(InspectSymbolRequest {
            symbol_handle: Some(handle),
            query: None,
            kind: None,
            path: None,
            container: None,
            candidate_limit: 10,
            sections: vec![crate::bridge::InspectSymbolSectionKind::Diagnostics],
            budget: crate::bridge::InspectSymbolBudget::default(),
        }))
        .await
        .unwrap();

        assert!(matches!(
            result.resolution,
            crate::bridge::InspectSymbolResolution::Selected { .. }
        ));
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
    #[allow(clippy::large_futures)]
    async fn project_actor_delivers_active_mutation_after_response_cancellation() {
        let (status_tx, _) = watch::channel(ProjectStatus::Starting);
        let (state_tx, _) = watch::channel(ProjectState::new(
            ProjectStatus::Starting,
            ProjectRuntimeSummary::default(),
        ));
        let (event_tx, _) = broadcast::channel(1);
        let channels = ProjectActorChannels {
            status_tx,
            state_tx,
            event_tx,
            event_history: std::sync::Arc::new(std::sync::Mutex::new(ProjectEventHistory::new(1))),
            gate: ProjectRequestGate::new(),
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
    async fn project_request_sender_attaches_queue_timing_before_enqueue() {
        let (channel, mut receiver) = mpsc::channel(1);
        let sender = ProjectRequestSender::new(channel);

        sender
            .send(ProjectRequest::ServerExited { generation: 0 })
            .await
            .unwrap();

        let request = receiver.recv().await.unwrap();
        let (request, timing) = request.into_timed();
        assert!(matches!(
            request,
            ProjectRequest::ServerExited { generation: 0 }
        ));
        assert!(timing.queued_at.elapsed() < Duration::from_secs(1));
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
        let ProjectRequest::Suspend { reply, .. } = suspend else {
            panic!("expected eviction only after the queued request completed");
        };
        reply.send(Ok(())).unwrap();

        second_send.await.unwrap().unwrap();
        assert!(matches!(
            second_receiver.recv().await.unwrap(),
            ProjectRequest::Resident { .. }
        ));
    }

    #[test]
    fn project_request_modes_distinguish_activation_from_activity() {
        let (query_reply, _query_response) = oneshot::channel();
        assert_eq!(
            ProjectRequest::Query { reply: query_reply }.rust_residency_mode(),
            Some(RustResidencyMode::Touch)
        );

        let (activate_reply, _activate_response) = oneshot::channel();
        assert_eq!(
            ProjectRequest::Activate {
                root: PathBuf::from("project"),
                reply: activate_reply,
            }
            .rust_residency_mode(),
            Some(RustResidencyMode::Activate)
        );
    }

    #[tokio::test]
    async fn touching_a_resident_project_request_does_not_wait_for_capacity() {
        let controller =
            RustResidencyController::with_idle_timeout(1, Duration::from_secs(60 * 60));
        let (channel, _receiver) = mpsc::channel(1);
        let residency = ProjectResidency {
            controller: controller.clone(),
            group: RustGroupId(1),
        };
        controller.register(RustGroupId(1), channel.downgrade());
        drop(controller.acquire(RustGroupId(1)).await);

        let (reply, _response) = oneshot::channel();
        let touched = residency.touch_request(ProjectRequest::Query { reply });
        assert!(matches!(touched, ProjectRequest::Resident { .. }));
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

        let snapshot = history.snapshot_since(Some(0), 2);
        assert!(snapshot.resync_required());
        assert_eq!(snapshot.events().len(), 2);
        assert_eq!(snapshot.events()[0].sequence(), 2);
        assert_eq!(snapshot.events()[1].sequence(), 3);
        assert_eq!(snapshot.next_sequence(), 3);

        history.record(ProjectEvent::ServerExited { generation: 2 });
        let resumed = history.snapshot_since(Some(snapshot.next_sequence()), 2);
        assert_eq!(resumed.events().len(), 1);
        assert_eq!(resumed.events()[0].sequence(), 4);
    }

    #[test]
    fn project_event_history_pages_an_exclusive_cursor_without_gaps() {
        let mut history = ProjectEventHistory::new(4);
        for generation in 1..=4 {
            history.record(ProjectEvent::ServerExited { generation });
        }

        let first = history.snapshot_since(None, 2);
        assert!(first.truncated());
        assert_eq!(first.next_sequence(), 2);
        assert_eq!(
            first
                .events()
                .iter()
                .map(ProjectEventRecord::sequence)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );

        let second = history.snapshot_since(Some(first.next_sequence()), 2);
        assert!(!second.truncated());
        assert_eq!(second.next_sequence(), 4);
        assert_eq!(
            second
                .events()
                .iter()
                .map(ProjectEventRecord::sequence)
                .collect::<Vec<_>>(),
            vec![3, 4]
        );
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

        let snapshot = history.snapshot_since(None, 4);
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
            .references(
                file.display().to_string(),
                0,
                0,
                false,
                SemanticResultLimits::default(),
            )
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

        let result = handle
            .document_symbols(file.display().to_string(), DocumentSymbolOptions::default())
            .await;

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
    async fn generated_preview_keeps_lsp_generation_and_snapshotting_in_one_actor_request() {
        let root = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let file = outside.path().join("outside.rs");
        fs::write(&file, "fn outside() {}\n").unwrap();
        let canonical_root = CanonicalRoot::new(root.path()).unwrap();
        let handle = spawn_project_actor_for_root(2, &canonical_root);

        let result = handle
            .preview_generated_edit(
                "project".to_owned(),
                GeneratedEditRequest::Format {
                    file_path: file.display().to_string(),
                    tab_size: 4,
                    insert_spaces: true,
                },
                PositionEncoding::Utf8,
                root.path().to_path_buf(),
            )
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
            .prepare_call_hierarchy(file.display().to_string(), 1, 5, None)
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
        let diagnostics = actor
            .cached_diagnostics(file.display().to_string())
            .await
            .unwrap();
        assert_eq!(
            diagnostics
                .cache
                .as_ref()
                .and_then(|cache| cache.document_version),
            None
        );
        assert!(diagnostics.cache.as_ref().is_some_and(|cache| cache.hit));
        assert_eq!(
            diagnostics
                .cache
                .as_ref()
                .and_then(|cache| cache.snapshot_identity.as_ref())
                .map(String::len),
            Some(64)
        );
    }

    #[tokio::test]
    async fn project_actor_pages_cached_diagnostics_with_snapshot_owned_cursors() {
        let root = TempDir::new().unwrap();
        let file = root.path().join("src.rs");
        fs::write(&file, "fn item() {}\n".repeat(300)).unwrap();
        let actor = spawn_project_actor_for_root(2, &CanonicalRoot::new(root.path()).unwrap());
        let uri = crate::bridge::path_to_uri(&file).unwrap();
        let diagnostics = (0..249)
            .map(|line| {
                serde_json::json!({
                    "range": {
                        "start": {"line": line, "character": 0},
                        "end": {"line": line, "character": 2}
                    },
                    "severity": 4,
                    "code": "macro-error",
                    "source": "rust-analyzer",
                    "message": "uniffi::constructor: internal café 🚗 proc-macro error"
                })
            })
            .collect::<Vec<_>>();
        actor
            .sender
            .send(ProjectRequest::Notification {
                generation: 0,
                server_id: ServerId::from("rust"),
                notification: LspNotification::parse(
                    "textDocument/publishDiagnostics",
                    Some(serde_json::json!({
                        "uri": uri,
                        "version": 7,
                        "diagnostics": diagnostics
                    })),
                ),
            })
            .await
            .unwrap();

        let mut options = DiagnosticOptions {
            preserve_locations: true,
            item_limit: 20,
            byte_limit: 6_000,
            ..DiagnosticOptions::default()
        };
        let mut lines = Vec::new();
        let mut snapshot_identity = None;
        let mut first_cursor = None;
        loop {
            let result = actor
                .cached_diagnostics_with_options(file.display().to_string(), options)
                .await
                .unwrap();
            assert!(serde_json::to_vec(&result).unwrap().len() <= 6_000);
            assert_eq!(result.total_diagnostics, 249);
            assert!(result.source_resource.is_some());
            if let Some(identity) = &snapshot_identity {
                assert_eq!(result.snapshot_identity.as_ref(), Some(identity));
            } else {
                snapshot_identity = result.snapshot_identity.clone();
            }
            lines.extend(
                result
                    .diagnostics
                    .iter()
                    .flat_map(|group| &group.context.occurrences)
                    .map(|occurrence| occurrence.range.start.line),
            );
            assert_eq!(result.remaining_diagnostics, 249 - lines.len());

            let Some(cursor) = result.next_cursor else {
                break;
            };
            first_cursor.get_or_insert_with(|| cursor.clone());
            options = DiagnosticOptions {
                page_token: Some(cursor),
                ..DiagnosticOptions::default()
            };
        }

        assert_eq!(lines, (1..=249).collect::<Vec<_>>());
        let mismatched = actor
            .diagnostics_with_options(
                file.display().to_string(),
                DiagnosticOptions {
                    page_token: first_cursor,
                    ..DiagnosticOptions::default()
                },
            )
            .await;
        assert!(matches!(
            mismatched,
            Err(ProjectActorError::Operation(message))
                if message.contains("different diagnostics request")
        ));
    }

    #[tokio::test]
    async fn project_actor_ignores_server_quiescence_until_initial_rust_indexing_finishes() {
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
                    "experimental/serverStatus",
                    Some(serde_json::json!({
                        "health": "ok",
                        "quiescent": true
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
        send({"jsonrpc": "2.0", "method": "$/progress",
              "params": {"token": "rustAnalyzer/Indexing", "value": {"kind": "end"}}})
    elif message.get("method") == "shutdown":
        send({"jsonrpc": "2.0", "id": message["id"], "result": None})
        break
"#;

    #[cfg(unix)]
    const CANCELLABLE_INITIALIZATION_LSP: &str = r#"#!/usr/bin/env python3
import os
import pathlib
import time

pathlib.Path(os.environ["MCPLS_PID_FILE"]).write_text(str(os.getpid()))
while True:
    time.sleep(1)
"#;

    #[cfg(unix)]
    const PROFILE_FAILURE_LSP: &str = r#"#!/usr/bin/env python3
import json
import sys

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
        if "bad-feature" in json.dumps(message.get("params")):
            sys.exit(2)
        send({"jsonrpc": "2.0", "id": message["id"], "result": {
            "capabilities": {"positionEncoding": "utf-8"}
        }})
        send({"jsonrpc": "2.0", "method": "experimental/serverStatus",
              "params": {"health": "ok", "quiescent": True}})
        send({"jsonrpc": "2.0", "method": "$/progress",
              "params": {"token": "rustAnalyzer/Indexing", "value": {"kind": "end"}}})
    elif message.get("method") == "shutdown":
        send({"jsonrpc": "2.0", "id": message["id"], "result": None})
        break
"#;

    fn write_compatible_roots_with_changed_manifests(roots: &[&Path]) {
        for root in roots {
            fs::write(
                root.join("rust-toolchain.toml"),
                "[toolchain]\nchannel = \"stable\"\n",
            )
            .unwrap();
        }
        let Some((first, linked)) = roots.split_first() else {
            return;
        };
        fs::write(
            first.join("Cargo.toml"),
            "[package]\nname = \"fixture-main\"\n",
        )
        .unwrap();
        fs::write(first.join("Cargo.lock"), "version = 3\n").unwrap();
        for root in linked {
            fs::write(
                root.join("Cargo.toml"),
                "[package]\nname = \"fixture-linked\"\n",
            )
            .unwrap();
            fs::write(
                root.join("Cargo.lock"),
                "version = 4\n\n[[package]]\nname = \"changed\"\nversion = \"1.0.0\"\n",
            )
            .unwrap();
        }
    }

    fn compatible_worktree_fixture() -> (TempDir, Vec<TempDir>, Vec<PathBuf>) {
        let repository = TempDir::new().unwrap();
        let git_dir = repository.path().join(".git");
        fs::create_dir_all(git_dir.join("objects")).unwrap();
        fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        fs::write(git_dir.join("config"), "[core]\n").unwrap();

        let worktrees: Vec<_> = (0..4)
            .map(|index| {
                let worktree_git_dir = git_dir.join("worktrees").join(format!("linked-{index}"));
                fs::create_dir_all(&worktree_git_dir).unwrap();
                fs::write(worktree_git_dir.join("commondir"), "../..\n").unwrap();
                let worktree = TempDir::new().unwrap();
                fs::write(
                    worktree.path().join(".git"),
                    format!("gitdir: {}\n", worktree_git_dir.display()),
                )
                .unwrap();
                worktree
            })
            .collect();
        let roots: Vec<_> = std::iter::once(repository.path().to_path_buf())
            .chain(
                worktrees
                    .iter()
                    .map(|worktree| worktree.path().to_path_buf()),
            )
            .collect();
        let root_refs: Vec<_> = roots.iter().map(PathBuf::as_path).collect();
        write_compatible_roots_with_changed_manifests(&root_refs);
        (repository, worktrees, roots)
    }

    async fn add_compatible_roots(
        registry: &ProjectRegistry,
        project_id: &ProjectId,
        roots: &[PathBuf],
    ) {
        for root in roots {
            let repository_identity = GitRepositoryIdentity::discover(root).unwrap().unwrap();
            registry
                .add(
                    ProjectIdentity::new(project_id.clone(), CanonicalRoot::new(root).unwrap())
                        .with_repository_identity(repository_identity),
                )
                .await
                .unwrap();
        }
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
        assert!(matches!(
            first.status(),
            ProjectStatus::Starting | ProjectStatus::Ready
        ));
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

    #[cfg(unix)]
    #[tokio::test]
    async fn degraded_activation_stays_degraded_after_initial_indexing() {
        use std::collections::HashMap;
        use std::os::unix::fs::PermissionsExt;

        let root = TempDir::new().unwrap();
        let counter = root.path().join("spawn-count");
        let lsp = root.path().join("ready-lsp.py");
        fs::write(&lsp, DUPLICATE_ACTIVATION_LSP).unwrap();
        let mut permissions = fs::metadata(&lsp).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&lsp, permissions).unwrap();

        let mut ready = crate::config::LspServerConfig::rust_analyzer();
        ready.command = lsp.display().to_string();
        ready.heuristics = None;
        ready.env = HashMap::from([(
            "MCPLS_SPAWN_COUNTER".to_string(),
            counter.display().to_string(),
        )]);
        let mut unavailable = ready.clone();
        unavailable.language_id = "unavailable".to_string();
        unavailable.command = "/definitely/missing/mcpls-lsp".to_string();
        unavailable.env.clear();

        let mut translator = Translator::new()
            .with_extensions(HashMap::from([("rs".to_string(), "rust".to_string())]));
        translator.set_workspace_roots(vec![root.path().to_path_buf()]);
        translator.set_lsp_configs(vec![ready, unavailable], Some(3));
        let actor = spawn_project_actor_with_translator(4, translator);

        actor.activate(root.path().to_path_buf()).await.unwrap();
        let state = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let state = actor.query().await.unwrap();
                if state.status() != ProjectStatus::Starting {
                    break state;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        assert_eq!(state.status(), ProjectStatus::Degraded);
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

    #[cfg(unix)]
    #[tokio::test]
    async fn project_actor_shutdown_cancels_pending_server_exit_recovery() {
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
        let mut translator = Translator::new();
        translator.set_workspace_roots(vec![root.path().to_path_buf()]);
        translator.set_lsp_configs(vec![config], Some(3));
        let actor = spawn_project_actor_with_translator(2, translator);

        actor.activate(root.path().to_path_buf()).await.unwrap();
        actor.set_status(ProjectStatus::Ready).await.unwrap();
        assert_eq!(fs::read_to_string(&counter).unwrap(), "1");

        actor
            .sender
            .send(ProjectRequest::ServerExited { generation: 1 })
            .await
            .unwrap();
        let shutdown = tokio::time::timeout(Duration::from_millis(50), actor.shutdown()).await;
        assert!(
            shutdown.is_ok(),
            "shutdown should cancel pending recovery promptly"
        );
        shutdown.unwrap().unwrap();

        assert_eq!(fs::read_to_string(&counter).unwrap(), "1");
        assert_eq!(actor.status().borrow().clone(), ProjectStatus::Stopped);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn project_actor_shutdown_cancels_initialization_and_reaps_lsp() {
        use std::collections::HashMap;
        use std::os::unix::fs::PermissionsExt;

        let root = TempDir::new().unwrap();
        let pid_file = root.path().join("lsp.pid");
        let lsp = root.path().join("blocked-lsp.py");
        fs::write(&lsp, CANCELLABLE_INITIALIZATION_LSP).unwrap();
        let mut permissions = fs::metadata(&lsp).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&lsp, permissions).unwrap();

        let mut config = crate::config::LspServerConfig::rust_analyzer();
        config.command = lsp.display().to_string();
        config.heuristics = None;
        config.timeout_seconds = 30;
        config.env =
            HashMap::from([("MCPLS_PID_FILE".to_string(), pid_file.display().to_string())]);
        let mut translator = Translator::new();
        translator.set_workspace_roots(vec![root.path().to_path_buf()]);
        translator.set_lsp_configs(vec![config], Some(3));
        let actor = spawn_project_actor_with_translator(2, translator);

        let activation = {
            let actor = actor.clone();
            let root = root.path().to_path_buf();
            tokio::spawn(async move { actor.activate(root).await })
        };
        let pid = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if let Ok(pid) = fs::read_to_string(&pid_file)
                    && let Ok(pid) = pid.trim().parse::<u32>()
                {
                    break pid;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        let shutdown = tokio::time::timeout(Duration::from_secs(1), actor.shutdown()).await;
        assert!(
            shutdown.is_ok(),
            "shutdown should cancel blocked initialization"
        );
        shutdown.unwrap().unwrap();
        assert!(activation.await.unwrap().is_err());
        assert!(!Path::new(&format!("/proc/{pid}")).exists());
        assert_eq!(actor.status().borrow().clone(), ProjectStatus::Stopped);
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
            .document_symbols(
                root.path().join("src/main.rs").display().to_string(),
                DocumentSymbolOptions::default(),
            )
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
    async fn structural_only_actor_reevaluates_lsp_when_linked_root_is_added() {
        let first = TempDir::new().unwrap();
        let second = TempDir::new().unwrap();
        fs::write(second.path().join("Cargo.toml"), "[workspace]\n").unwrap();
        let mut config = crate::config::LspServerConfig::rust_analyzer();
        config.command = "/definitely/missing/mcpls-language-server".to_string();
        let mut translator = Translator::new();
        translator.set_workspace_roots(vec![first.path().to_path_buf()]);
        translator.set_lsp_configs(vec![config], Some(1));
        let actor = spawn_project_actor_with_translator(2, translator);

        let initial = actor.activate(first.path().to_path_buf()).await.unwrap();
        let added = actor.add_workspace_root(second.path().to_path_buf()).await;

        assert_eq!(initial.status(), ProjectStatus::Degraded);
        assert!(matches!(added, Err(ProjectActorError::Operation(_))));
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
    async fn project_runtime_refuses_to_replace_disk_with_dirty_open_document_content() {
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
        assert!(!artifact.plan.safe_to_apply());
        assert!(
            artifact
                .conflicts
                .iter()
                .any(|conflict| conflict.contains("open document differs from disk"))
        );
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
        let error = runtime
            .apply_edit_plan_with_context(&plan_id, "project", root.path(), None, None)
            .await
            .unwrap_err();
        assert_eq!(error, "edit plan is not safe to apply");
        assert_eq!(
            runtime
                .translator
                .document_tracker()
                .get(&source)
                .unwrap()
                .content(),
            dirty
        );
        assert_eq!(
            fs::read_to_string(source).unwrap(),
            "pub mod feature { fn disk() {} }\n"
        );
        assert!(!destination.exists());
    }

    #[tokio::test]
    async fn dirty_documents_refuse_residency_suspension() {
        let root = TempDir::new().unwrap();
        let source = root.path().join("lib.rs");
        fs::write(&source, "pub fn on_disk() {}\n").unwrap();

        let mut translator = Translator::new();
        translator.set_workspace_roots(vec![root.path().to_path_buf()]);
        translator
            .document_tracker_mut()
            .open(source, "pub fn unsaved() {}\n".to_string())
            .unwrap();
        let mut runtime = ProjectRuntime::new(translator);
        let (status_tx, _) = watch::channel(ProjectStatus::Ready);
        let (state_tx, _) =
            watch::channel(ProjectState::new(ProjectStatus::Ready, runtime.summary()));
        let (event_tx, _) = broadcast::channel(1);
        let channels = ProjectActorChannels {
            status_tx,
            state_tx,
            event_tx,
            event_history: std::sync::Arc::new(std::sync::Mutex::new(ProjectEventHistory::new(1))),
            gate: ProjectRequestGate::new(),
        };
        let mut state = ProjectState::new(ProjectStatus::Ready, runtime.summary());

        assert!(
            suspend_project_runtime(
                &channels,
                &mut state,
                &mut runtime,
                ProjectDormancy::new(ProjectDormancyReason::Restored, None),
            )
            .await
            .is_err()
        );
        assert_eq!(state.status(), ProjectStatus::Ready);
        assert_eq!(runtime.summary().open_document_count(), 1);
    }

    #[tokio::test]
    async fn residency_suspension_records_dormancy_reason_and_idle_duration() {
        let mut runtime = ProjectRuntime::new(Translator::new());
        let (status_tx, _) = watch::channel(ProjectStatus::Ready);
        let (state_tx, _) =
            watch::channel(ProjectState::new(ProjectStatus::Ready, runtime.summary()));
        let (event_tx, _) = broadcast::channel(1);
        let channels = ProjectActorChannels {
            status_tx,
            state_tx,
            event_tx,
            event_history: std::sync::Arc::new(std::sync::Mutex::new(ProjectEventHistory::new(1))),
            gate: ProjectRequestGate::new(),
        };
        let mut state = ProjectState::new(ProjectStatus::Ready, runtime.summary());
        let idle_for = Duration::from_secs(60 * 60);

        suspend_project_runtime(
            &channels,
            &mut state,
            &mut runtime,
            ProjectDormancy::new(ProjectDormancyReason::ResidencyEviction, Some(idle_for)),
        )
        .await
        .unwrap();

        let Some(dormancy) = state.dormancy() else {
            panic!("residency suspension should report dormancy");
        };
        assert_eq!(dormancy.reason(), ProjectDormancyReason::ResidencyEviction);
        assert_eq!(dormancy.idle_for(), Some(idle_for));
    }

    #[tokio::test]
    async fn path_rename_composition_uses_authoritative_open_document_content() {
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
            .await
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

    #[tokio::test]
    async fn preview_edit_refreshes_clean_tracked_document_after_external_rewrite() {
        let root = TempDir::new().unwrap();
        let file = root.path().join("source.rs");
        fs::write(&file, "before\n").unwrap();
        let mut translator = Translator::new();
        translator.set_workspace_roots(vec![root.path().to_path_buf()]);
        let tracker = translator.document_tracker();
        tracker.open(file.clone(), "before\n".to_owned()).unwrap();
        tracker.reconciled_snapshot(&file).await.unwrap();

        fs::write(&file, "after!\n").unwrap();
        let edit = serde_json::from_value(serde_json::json!({
            "changes": {
                path_to_uri(&file).unwrap().to_string(): [{
                    "range": {
                        "start": {"line": 0, "character": 0},
                        "end": {"line": 0, "character": 5}
                    },
                    "newText": "fresh"
                }]
            }
        }))
        .unwrap();

        let mut runtime = ProjectRuntime::new(translator);
        let artifact = runtime
            .preview_edit("project", edit, PositionEncoding::Utf8, root.path())
            .await
            .unwrap();

        assert!(artifact.plan.safe_to_apply(), "{:?}", artifact.conflicts);
        assert_eq!(artifact.plan.files()[0].original_content(), "after!\n");
        assert_eq!(artifact.plan.files()[0].planned_content(), "fresh!\n");
    }

    #[tokio::test]
    async fn preview_edit_preserves_dirty_document_on_external_rewrite() {
        let root = TempDir::new().unwrap();
        let file = root.path().join("source.rs");
        fs::write(&file, "external\n").unwrap();
        let mut translator = Translator::new();
        translator.set_workspace_roots(vec![root.path().to_path_buf()]);
        translator
            .document_tracker_mut()
            .open(file.clone(), "local\n".to_owned())
            .unwrap();

        fs::write(&file, "external rewrite\n").unwrap();
        let edit = serde_json::from_value(serde_json::json!({
            "changes": {
                path_to_uri(&file).unwrap().to_string(): [{
                    "range": {
                        "start": {"line": 0, "character": 0},
                        "end": {"line": 0, "character": 5}
                    },
                    "newText": "fresh"
                }]
            }
        }))
        .unwrap();

        let mut runtime = ProjectRuntime::new(translator);
        let artifact = runtime
            .preview_edit("project", edit, PositionEncoding::Utf8, root.path())
            .await
            .unwrap();

        assert!(!artifact.plan.safe_to_apply());
        assert!(
            artifact
                .conflicts
                .iter()
                .any(|conflict| conflict.contains("open document differs from disk"))
        );
        assert_eq!(
            runtime
                .translator
                .document_tracker()
                .get(&file)
                .unwrap()
                .content(),
            "local\n"
        );
        assert_eq!(fs::read_to_string(file).unwrap(), "external rewrite\n");
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
    async fn committed_edit_plan_retry_returns_the_original_receipt() {
        let root = TempDir::new().unwrap();
        let file = root.path().join("retry.rs");
        fs::write(&file, "before\n").unwrap();
        let mut runtime = ProjectRuntime::new(Translator::new());
        let plan = EditPlan::new(
            "project".to_string(),
            vec![crate::edit_plan::FileSnapshot::from_contents(
                file.clone(),
                crate::edit_plan::SnapshotSource::Disk,
                None,
                "before\n",
                "after\n",
            )],
            vec!["replace retry fixture".to_string()],
            true,
            Duration::from_secs(60),
        );
        let plan_id = plan.id().clone();
        runtime.store_edit_plan(plan).unwrap();

        let first = runtime
            .apply_edit_plan_with_context(&plan_id, "project", root.path(), None, None)
            .await
            .unwrap();
        let retry = runtime
            .apply_edit_plan_with_context(&plan_id, "project", root.path(), None, None)
            .await
            .unwrap();

        assert_eq!(retry, first);
        assert_eq!(fs::read_to_string(file).unwrap(), "after\n");
        assert_eq!(runtime.edit_plans.audit_records().count(), 1);
    }

    #[tokio::test]
    async fn registry_overlaps_disjoint_same_project_commits() {
        let root = TempDir::new().unwrap();
        let first_path = root.path().join("first.rs");
        let second_path = root.path().join("second.rs");
        fs::write(&first_path, "before first\n").unwrap();
        fs::write(&second_path, "before second\n").unwrap();

        let registry = ProjectRegistry::new(4);
        let project_id = ProjectId::new("project").unwrap();
        let actor = registry
            .add(ProjectIdentity::new(
                project_id.clone(),
                CanonicalRoot::new(root.path()).unwrap(),
            ))
            .await
            .unwrap();
        let first = EditPlan::new(
            project_id.to_string(),
            vec![crate::edit_plan::FileSnapshot::from_contents(
                first_path.clone(),
                crate::edit_plan::SnapshotSource::Disk,
                None,
                "before first\n",
                "after first\n",
            )],
            vec!["replace first".to_owned()],
            true,
            Duration::from_secs(60),
        )
        .with_workspace_root(root.path().to_path_buf());
        let second = EditPlan::new(
            project_id.to_string(),
            vec![crate::edit_plan::FileSnapshot::from_contents(
                second_path.clone(),
                crate::edit_plan::SnapshotSource::Disk,
                None,
                "before second\n",
                "after second\n",
            )],
            vec!["replace second".to_owned()],
            true,
            Duration::from_secs(60),
        )
        .with_workspace_root(root.path().to_path_buf());
        let first_id = first.id().clone();
        let second_id = second.id().clone();
        actor.store_edit_plan(first).await.unwrap();
        actor.store_edit_plan(second).await.unwrap();

        let _barrier =
            crate::edit_apply::install_test_apply_barrier([first_id.clone(), second_id.clone()], 2);
        let (first_result, second_result) = tokio::join!(
            registry.apply_edit_plan_with_context(
                &project_id,
                first_id,
                Some("first-session".to_owned()),
                None,
            ),
            registry.apply_edit_plan_with_context(
                &project_id,
                second_id,
                Some("second-session".to_owned()),
                None,
            ),
        );
        assert!(matches!(
            first_result.unwrap(),
            ApplyEditPlanOutcome::Applied(_)
        ));
        assert!(matches!(
            second_result.unwrap(),
            ApplyEditPlanOutcome::Applied(_)
        ));
        assert_eq!(fs::read_to_string(first_path).unwrap(), "after first\n");
        assert_eq!(fs::read_to_string(second_path).unwrap(), "after second\n");
    }

    #[tokio::test]
    async fn registry_overlaps_linked_worktree_commits() {
        let (_repository, _worktrees, roots) = compatible_worktree_fixture();
        let registry = ProjectRegistry::new(4);
        let project_id = ProjectId::new("project").unwrap();
        add_compatible_roots(&registry, &project_id, &roots).await;
        let actor = registry.actor_for_project(&project_id).await.unwrap();
        let first_path = roots[0].join("src.rs");
        let second_path = roots[1].join("src.rs");
        fs::write(&first_path, "before first\n").unwrap();
        fs::write(&second_path, "before second\n").unwrap();
        let first = EditPlan::new(
            project_id.to_string(),
            vec![crate::edit_plan::FileSnapshot::from_contents(
                first_path.clone(),
                crate::edit_plan::SnapshotSource::Disk,
                None,
                "before first\n",
                "after first\n",
            )],
            vec!["replace first".to_owned()],
            true,
            Duration::from_secs(60),
        )
        .with_workspace_root(roots[0].clone());
        let second = EditPlan::new(
            project_id.to_string(),
            vec![crate::edit_plan::FileSnapshot::from_contents(
                second_path.clone(),
                crate::edit_plan::SnapshotSource::Disk,
                None,
                "before second\n",
                "after second\n",
            )],
            vec!["replace second".to_owned()],
            true,
            Duration::from_secs(60),
        )
        .with_workspace_root(roots[1].clone());
        let first_id = first.id().clone();
        let second_id = second.id().clone();
        actor.store_edit_plan(first).await.unwrap();
        actor.store_edit_plan(second).await.unwrap();

        let _barrier =
            crate::edit_apply::install_test_apply_barrier([first_id.clone(), second_id.clone()], 2);
        let (first_result, second_result) = tokio::join!(
            registry.apply_edit_plan_with_context(
                &project_id,
                first_id,
                Some("first-session".to_owned()),
                None,
            ),
            registry.apply_edit_plan_with_context(
                &project_id,
                second_id,
                Some("second-session".to_owned()),
                None,
            ),
        );
        assert!(matches!(
            first_result.unwrap(),
            ApplyEditPlanOutcome::Applied(_)
        ));
        assert!(matches!(
            second_result.unwrap(),
            ApplyEditPlanOutcome::Applied(_)
        ));
        assert_eq!(fs::read_to_string(first_path).unwrap(), "after first\n");
        assert_eq!(fs::read_to_string(second_path).unwrap(), "after second\n");
    }

    #[tokio::test]
    async fn registry_reports_busy_without_consuming_a_plan() {
        let root = TempDir::new().unwrap();
        let file = root.path().join("busy.rs");
        fs::write(&file, "before\n").unwrap();
        let registry = ProjectRegistry::new(2);
        let project_id = ProjectId::new("project").unwrap();
        let actor = registry
            .add(ProjectIdentity::new(
                project_id.clone(),
                CanonicalRoot::new(root.path()).unwrap(),
            ))
            .await
            .unwrap();
        let plan = EditPlan::new(
            project_id.to_string(),
            vec![crate::edit_plan::FileSnapshot::from_contents(
                file.clone(),
                crate::edit_plan::SnapshotSource::Disk,
                None,
                "before\n",
                "after\n",
            )],
            vec!["replace busy".to_owned()],
            true,
            Duration::from_secs(60),
        )
        .with_workspace_root(root.path().to_path_buf());
        let plan_id = plan.id().clone();
        actor.store_edit_plan(plan).await.unwrap();
        let blocker = registry
            .edit_coordinator
            .try_acquire(
                "blocker",
                [crate::edit_coordinator::EditResource::exact(file)],
            )
            .unwrap();

        let busy = registry
            .apply_edit_plan_with_wait(&project_id, plan_id.clone(), None, None, Duration::ZERO)
            .await
            .unwrap();
        assert!(matches!(busy, ApplyEditPlanOutcome::NotReady(_)));
        assert!(
            actor
                .inspect_edit_plan(plan_id.clone(), project_id.to_string())
                .await
                .is_ok()
        );

        drop(blocker);
        assert!(matches!(
            registry
                .apply_edit_plan_with_context(&project_id, plan_id, None, None)
                .await
                .unwrap(),
            ApplyEditPlanOutcome::Applied(_)
        ));
    }

    #[tokio::test]
    async fn registry_competing_same_file_is_retryable_then_conflicts() {
        let root = TempDir::new().unwrap();
        let file = root.path().join("src.rs");
        fs::write(&file, "before\n").unwrap();
        let registry = ProjectRegistry::new(2);
        let project_id = ProjectId::new("project").unwrap();
        let actor = registry
            .add(ProjectIdentity::new(
                project_id.clone(),
                CanonicalRoot::new(root.path()).unwrap(),
            ))
            .await
            .unwrap();
        let plan = |after| {
            EditPlan::new(
                project_id.to_string(),
                vec![crate::edit_plan::FileSnapshot::from_contents(
                    file.clone(),
                    crate::edit_plan::SnapshotSource::Disk,
                    None,
                    "before\n",
                    after,
                )],
                vec!["replace src.rs".to_owned()],
                true,
                Duration::from_secs(60),
            )
            .with_workspace_root(root.path().to_path_buf())
        };
        let first = plan("first\n");
        let second = plan("second\n");
        let first_id = first.id().clone();
        let second_id = second.id().clone();
        actor.store_edit_plan(first).await.unwrap();
        actor.store_edit_plan(second).await.unwrap();

        let lease = registry
            .edit_coordinator
            .try_acquire(
                "first-session",
                [crate::edit_coordinator::EditResource::exact(file.clone())],
            )
            .unwrap();
        let busy = registry
            .apply_edit_plan_with_wait(
                &project_id,
                second_id.clone(),
                Some("second-session".to_owned()),
                None,
                Duration::ZERO,
            )
            .await
            .unwrap();
        assert!(matches!(busy, ApplyEditPlanOutcome::NotReady(_)));
        assert!(
            actor
                .inspect_edit_plan(second_id.clone(), project_id.to_string())
                .await
                .is_ok()
        );

        drop(lease);
        assert!(matches!(
            registry
                .apply_edit_plan_with_context(
                    &project_id,
                    first_id,
                    Some("first-session".to_owned()),
                    None,
                )
                .await
                .unwrap(),
            ApplyEditPlanOutcome::Applied(_)
        ));
        assert!(matches!(
            registry
                .apply_edit_plan_with_context(
                    &project_id,
                    second_id,
                    Some("second-session".to_owned()),
                    None,
                )
                .await
                .unwrap(),
            ApplyEditPlanOutcome::Conflict(_)
        ));
        assert_eq!(fs::read_to_string(file).unwrap(), "first\n");
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
        write_compatible_roots_with_changed_manifests(&[repository.path(), worktree.path()]);
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
        config.command = "/definitely/missing/custom-rust-lsp".to_string();
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
                .event_snapshot(None, 256)
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
    async fn project_registry_updates_cargo_features_without_replacing_identity() {
        let root = TempDir::new().unwrap();
        let project_id = ProjectId::new("cargo-profile").unwrap();
        let identity =
            ProjectIdentity::new(project_id.clone(), CanonicalRoot::new(root.path()).unwrap());
        let registry = ProjectRegistry::new(2);
        let actor = registry.add(identity.clone()).await.unwrap();
        let plan = EditPlan::new(
            project_id.to_string(),
            vec![crate::edit_plan::FileSnapshot::from_contents(
                root.path().join("lib.rs"),
                crate::edit_plan::SnapshotSource::Disk,
                None,
                "fn stale_target() {}\n",
                "fn fresh_target() {}\n",
            )],
            vec!["replace stale target".to_owned()],
            true,
            Duration::from_secs(60),
        );
        let plan_id = plan.id().clone();
        actor.store_edit_plan(plan).await.unwrap();

        let profile = crate::config::CargoFeatureProfile {
            features: vec!["serde".to_owned(), "alloc".to_owned(), "serde".to_owned()],
            all_features: false,
            no_default_features: true,
        };
        registry
            .update_cargo_features(&project_id, profile.clone())
            .await
            .unwrap();

        assert_eq!(registry.identity(&project_id).await.unwrap(), identity);
        assert_eq!(
            registry.cargo_features(&project_id).await.unwrap(),
            Some(profile.normalized())
        );
        let replacement = registry.actor_for_project(&project_id).await.unwrap();
        assert!(
            replacement
                .inspect_edit_plan(plan_id, project_id.to_string())
                .await
                .is_err()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cargo_feature_update_rolls_back_after_failed_activation() {
        use std::os::unix::fs::PermissionsExt;

        let root = TempDir::new().unwrap();
        fs::write(
            root.path().join("Cargo.toml"),
            "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        )
        .unwrap();
        fs::write(
            root.path().join("rust-toolchain.toml"),
            "[toolchain]\nchannel = \"stable\"\n",
        )
        .unwrap();
        let lsp = root.path().join("profile-failure-lsp.py");
        fs::write(&lsp, PROFILE_FAILURE_LSP).unwrap();
        let mut permissions = fs::metadata(&lsp).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&lsp, permissions).unwrap();

        let mut server = crate::config::LspServerConfig::rust_analyzer();
        server.command = lsp.display().to_string();
        server.heuristics = None;
        let old_profile = crate::config::CargoFeatureProfile {
            features: vec!["good-feature".to_owned()],
            all_features: false,
            no_default_features: false,
        };
        let registry = ProjectRegistry::new(4);
        let project_id = ProjectId::new("rollback-profile").unwrap();
        let actor = registry
            .add_with_config(
                ProjectIdentity::new(project_id.clone(), CanonicalRoot::new(root.path()).unwrap()),
                Some(ProjectConfig {
                    lsp_servers: Some(vec![server]),
                    heuristics_max_depth: Some(3),
                    redaction_patterns: None,
                    persist_environment: false,
                    edit_safety: None,
                    cargo_features: Some(old_profile.clone()),
                }),
            )
            .await
            .unwrap();
        let state = registry.activate(&project_id).await.unwrap();
        assert!(matches!(
            state.status(),
            ProjectStatus::Starting | ProjectStatus::Ready
        ));
        let state = tokio::time::timeout(Duration::from_secs(3), async {
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
        assert_eq!(state.status(), ProjectStatus::Ready);

        let result = registry
            .update_cargo_features(
                &project_id,
                crate::config::CargoFeatureProfile {
                    features: vec!["bad-feature".to_owned()],
                    all_features: false,
                    no_default_features: false,
                },
            )
            .await;

        assert!(matches!(
            result,
            Err(ProjectRegistryError::Actor(ProjectActorError::Operation(_)))
        ));
        assert_eq!(
            registry.cargo_features(&project_id).await.unwrap(),
            Some(old_profile)
        );
        let current = registry.actor_for_project(&project_id).await.unwrap();
        assert!(current.sender.same_channel(&actor.sender));
        assert_eq!(
            current.query().await.unwrap().status(),
            ProjectStatus::Ready
        );
    }

    #[tokio::test]
    async fn project_registry_restores_persisted_cargo_feature_profile() {
        let root = TempDir::new().unwrap();
        fs::write(
            root.path().join("Cargo.toml"),
            "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        )
        .unwrap();
        let store = ProjectRegistrationStore::new(root.path().join("state/projects.json"));
        let profile = crate::config::CargoFeatureProfile {
            features: vec!["serde".to_owned(), "alloc".to_owned()],
            all_features: false,
            no_default_features: true,
        };
        let registry = ProjectRegistry::new(2).with_persistence(store.clone());
        registry
            .add_with_config(
                ProjectIdentity::new(
                    ProjectId::new("persisted-profile").unwrap(),
                    CanonicalRoot::new(root.path()).unwrap(),
                ),
                Some(ProjectConfig {
                    cargo_features: Some(profile.clone()),
                    ..ProjectConfig::default()
                }),
            )
            .await
            .unwrap();

        let restored = ProjectRegistry::new(2).with_persistence(store);
        assert_eq!(restored.restore_from_persistence().await.unwrap(), 1);
        assert_eq!(
            restored
                .cargo_features(&ProjectId::new("persisted-profile").unwrap())
                .await
                .unwrap(),
            Some(profile.normalized())
        );
    }

    #[tokio::test]
    async fn cargo_feature_update_invalidates_only_that_projects_deferred_resources() {
        let first_root = TempDir::new().unwrap();
        let second_root = TempDir::new().unwrap();
        let first_id = ProjectId::new("first-profile").unwrap();
        let second_id = ProjectId::new("second-profile").unwrap();
        let registry = ProjectRegistry::new(2);
        registry
            .add(ProjectIdentity::new(
                first_id.clone(),
                CanonicalRoot::new(first_root.path()).unwrap(),
            ))
            .await
            .unwrap();
        registry
            .add(ProjectIdentity::new(
                second_id.clone(),
                CanonicalRoot::new(second_root.path()).unwrap(),
            ))
            .await
            .unwrap();
        let first_reference = registry.deferred_results.lock().unwrap().insert_scoped(
            serde_json::json!("first"),
            "first".to_owned(),
            first_id.as_str(),
        );
        let second_reference = registry.deferred_results.lock().unwrap().insert_scoped(
            serde_json::json!("second"),
            "second".to_owned(),
            second_id.as_str(),
        );
        let first_token = first_reference
            .uri
            .strip_prefix("mcpls-deferred:///")
            .unwrap();
        let second_token = second_reference
            .uri
            .strip_prefix("mcpls-deferred:///")
            .unwrap();

        registry
            .update_cargo_features(
                &first_id,
                crate::config::CargoFeatureProfile {
                    features: vec!["serde".to_owned()],
                    all_features: false,
                    no_default_features: false,
                },
            )
            .await
            .unwrap();

        assert!(registry.read_deferred_resource(first_token).is_err());
        assert_eq!(
            registry.read_deferred_resource(second_token).unwrap().value,
            serde_json::json!("second")
        );
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
        let (_, state) = watch::channel(ProjectState::new(
            ProjectStatus::Starting,
            ProjectRuntimeSummary::default(),
        ));
        let (events, _) = broadcast::channel(1);
        registry.projects.write().await.insert(
            project_id.clone(),
            ProjectEntry {
                identity: identity.clone(),
                actors: vec![ProjectActorEntry {
                    actor: ProjectHandle {
                        sender: ProjectRequestSender::new(sender),
                        status,
                        state,
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
        let (sender, mut receiver) = mpsc::channel::<ProjectRequest>(2);
        tokio::spawn(async move {
            while let Some(request) = receiver.recv().await {
                let (request, _) = request.into_timed();
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
        let (_, state) = watch::channel(ProjectState::new(
            ProjectStatus::Starting,
            ProjectRuntimeSummary::default(),
        ));
        let (events, _) = broadcast::channel(1);
        let actor = ProjectHandle {
            sender: ProjectRequestSender::new(sender),
            status,
            state,
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
        assert_eq!(
            registry
                .status(&ProjectId::new("existing").unwrap())
                .await
                .unwrap()
                .status(),
            ProjectStatus::Dormant
        );
        assert_eq!(store.load().unwrap().projects.len(), 1);
    }

    #[tokio::test]
    async fn active_actor_for_path_wakes_a_dormant_restored_project() {
        let root = TempDir::new().unwrap();
        let file = root.path().join("main.rs");
        std::fs::write(&file, "fn main() {}\n").unwrap();
        let store = ProjectRegistrationStore::new(root.path().join("state/projects.json"));
        store
            .save(&[PersistedProject {
                project_id: "dormant".to_owned(),
                root: root.path().to_path_buf(),
                additional_roots: Vec::new(),
                config: None,
            }])
            .unwrap();
        let registry = ProjectRegistry::new(2).with_persistence(store);
        registry.restore_from_persistence().await.unwrap();

        registry.active_actor_for_path(&file).await.unwrap();

        assert_ne!(
            registry
                .status(&ProjectId::new("dormant").unwrap())
                .await
                .unwrap()
                .status(),
            ProjectStatus::Dormant
        );
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
        write_compatible_roots_with_changed_manifests(&[repository.path(), worktree.path()]);
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
            cargo_features: None,
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
        config.command = "/definitely/missing/custom-rust-lsp".to_string();
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
                    cargo_features: None,
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
        let (_, state) = watch::channel(ProjectState::new(
            ProjectStatus::Starting,
            ProjectRuntimeSummary::default(),
        ));
        let (events, _) = broadcast::channel(1);
        let actor = ProjectHandle {
            sender: ProjectRequestSender::new(sender),
            status,
            state,
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

    #[tokio::test]
    async fn linked_worktrees_share_only_when_cargo_profiles_match() {
        let (repository, worktrees, roots) = compatible_worktree_fixture();
        let project_id = ProjectId::new("profile-linked").unwrap();
        let mut server = crate::config::LspServerConfig::rust_analyzer();
        server.command = "rust-analyzer".to_owned();
        server.heuristics = None;
        let mut template_source = Translator::new();
        template_source.set_lsp_configs(vec![server], Some(3));
        let registry =
            ProjectRegistry::with_translator_template(4, template_source.configuration_template());
        let same_profile = ProjectConfig {
            cargo_features: Some(crate::config::CargoFeatureProfile {
                features: vec!["shared".to_owned()],
                all_features: false,
                no_default_features: false,
            }),
            ..ProjectConfig::default()
        };
        let different_profile = ProjectConfig {
            cargo_features: Some(crate::config::CargoFeatureProfile {
                features: vec!["isolated".to_owned()],
                all_features: false,
                no_default_features: false,
            }),
            ..ProjectConfig::default()
        };
        let repository_identity = GitRepositoryIdentity::discover(repository.path())
            .unwrap()
            .unwrap();
        registry
            .add_with_config(
                ProjectIdentity::new(project_id.clone(), CanonicalRoot::new(&roots[0]).unwrap())
                    .with_repository_identity(repository_identity),
                Some(same_profile.clone()),
            )
            .await
            .unwrap();

        let linked_identity = GitRepositoryIdentity::discover(worktrees[0].path())
            .unwrap()
            .unwrap();
        let shared = registry
            .add_with_config(
                ProjectIdentity::new(
                    project_id.clone(),
                    CanonicalRoot::new(worktrees[0].path()).unwrap(),
                )
                .with_repository_identity(linked_identity),
                Some(same_profile),
            )
            .await
            .unwrap();
        assert_eq!(registry.actor_group_count(&project_id).await.unwrap(), 1);
        assert_eq!(shared.query().await.unwrap().workspace_roots().len(), 2);

        let isolated_identity = GitRepositoryIdentity::discover(worktrees[1].path())
            .unwrap()
            .unwrap();
        let isolated = registry
            .add_with_config(
                ProjectIdentity::new(
                    project_id.clone(),
                    CanonicalRoot::new(worktrees[1].path()).unwrap(),
                )
                .with_repository_identity(isolated_identity),
                Some(different_profile),
            )
            .await
            .unwrap();
        assert_eq!(registry.actor_group_count(&project_id).await.unwrap(), 2);
        assert!(!shared.sender.same_channel(&isolated.sender));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn compatible_linked_worktrees_share_one_lsp_process() {
        use std::collections::HashMap;
        use std::os::unix::fs::PermissionsExt;

        let (repository, _worktrees, roots) = compatible_worktree_fixture();
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
        add_compatible_roots(&registry, &project_id, &roots).await;

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
        assert_eq!(state.workspace_roots().len(), 5);
        assert_eq!(registry.actor_group_count(&project_id).await.unwrap(), 1);
        assert_eq!(fs::read_to_string(counter).unwrap(), "1");
    }

    #[tokio::test]
    async fn twenty_registered_worktrees_remain_four_idle_actor_groups() {
        let mut template_source = Translator::new().with_extensions(
            std::collections::HashMap::from([("rs".to_string(), "rust".to_string())]),
        );
        template_source.set_lsp_configs(
            vec![crate::config::LspServerConfig::rust_analyzer()],
            Some(3),
        );
        let registry =
            ProjectRegistry::with_translator_template(4, template_source.configuration_template())
                .with_rust_residency_limit(1);
        let mut fixtures = Vec::new();

        for index in 0..4 {
            let (repository, worktrees, roots) = compatible_worktree_fixture();
            let project_id = ProjectId::new(format!("repository-{index}")).unwrap();
            add_compatible_roots(&registry, &project_id, &roots).await;
            fixtures.push((repository, worktrees));
        }

        let snapshot = registry.status_snapshot().await;
        assert_eq!(registry.list().await.len(), 4);
        assert_eq!(registry.total_actor_group_count().await, 4);
        assert_eq!(snapshot.actor_groups, 4);
        assert_eq!(snapshot.counts.starting, 0);
        assert_eq!(snapshot.counts.ready + snapshot.counts.dormant, 4);
        assert_eq!(snapshot.counts.failed, 0);
        assert_eq!(snapshot.queue_pressure.queued, 0);
        assert!(
            snapshot
                .summaries
                .iter()
                .all(|summary| summary.actor_group_count == 1)
        );
        assert_eq!(
            snapshot
                .summaries
                .iter()
                .map(|summary| summary.roots.len())
                .sum::<usize>(),
            20
        );
        drop(fixtures);
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
