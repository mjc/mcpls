//! Project identity and canonical path routing primitives.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use tokio::sync::{Mutex, RwLock, broadcast, mpsc, oneshot, watch};

use crate::bridge::convert_code_action_or_command;
use crate::bridge::{
    CallHierarchyPrepareResult, CodeActionsResult, CompletionsResult, DefinitionResult,
    DiagnosticsResult, DocumentSymbolsResult, FormatDocumentResult, HoverResult,
    IncomingCallsResult, InlayHintsResult, LocationsResult, OutgoingCallsResult, PositionEncoding,
    ReferencesResult, RenameResult, ServerLogsResult, ServerMessagesResult, SignatureHelpResult,
    Translator, TranslatorTemplate, WorkspaceSymbolResult,
};
use crate::edit_apply::{ApplyReport, apply_plan_with_documents};
use crate::edit_paths::WorkspaceBoundary;
use crate::edit_plan::{EditPlan, EditPlanStore, PlanId};
use crate::edit_preview::{PreviewArtifact, PreviewLimits, preview_workspace_edit};
use crate::lsp::LspNotification;
use crate::project_persistence::{PersistedProject, ProjectRegistrationStore};
use lsp_types::WorkspaceEdit;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ProjectCompatibilityKey([u8; 32]);

/// Return a conservative fingerprint for the inputs that shape Rust analysis.
///
/// A missing explicit toolchain or Cargo manifest is deliberately treated as
/// unknown rather than compatible. This keeps linked-project reuse fail-closed
/// until the daemon can resolve the effective toolchain and environment.
fn rust_project_compatibility_key(root: &Path) -> Option<ProjectCompatibilityKey> {
    const INPUTS: &[&str] = &[
        "rust-toolchain",
        "rust-toolchain.toml",
        "Cargo.toml",
        "Cargo.lock",
        ".cargo/config",
        ".cargo/config.toml",
    ];

    let mut hasher = Sha256::new();
    let mut has_toolchain = false;
    let mut has_manifest = false;
    for relative in INPUTS {
        let path = root.join(relative);
        match std::fs::read(&path) {
            Ok(contents) => {
                has_toolchain |=
                    *relative == "rust-toolchain" || *relative == "rust-toolchain.toml";
                has_manifest |= *relative == "Cargo.toml";
                hasher.update(relative.as_bytes());
                hasher.update((contents.len() as u64).to_le_bytes());
                hasher.update(contents);
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {
                hasher.update(relative.as_bytes());
                hasher.update([0]);
            }
            Err(_) => return None,
        }
    }

    (has_toolchain && has_manifest).then(|| ProjectCompatibilityKey(hasher.finalize().into()))
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
    /// The actor is draining work before shutdown.
    Stopping,
    /// The actor has stopped and accepts no new requests.
    Stopped,
    /// The project failed and requires recovery or explicit restart.
    Failed,
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
        let mut aggregate = Self::new(ProjectStatus::Starting, ProjectRuntimeSummary::default());
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
        self.runtime
            .workspace_roots
            .extend(state.runtime.workspace_roots);
        self.runtime
            .configured_language_ids
            .extend(state.runtime.configured_language_ids);
        self.runtime
            .active_language_ids
            .extend(state.runtime.active_language_ids);
        self.runtime.open_document_count += state.runtime.open_document_count;
        self.runtime.workspace_roots.sort();
        self.runtime.workspace_roots.dedup();
        self.runtime.configured_language_ids.sort();
        self.runtime.configured_language_ids.dedup();
        self.runtime.active_language_ids.sort();
        self.runtime.active_language_ids.dedup();
    }
}

fn project_status_priority(status: ProjectStatus) -> u8 {
    match status {
        ProjectStatus::Failed => 6,
        ProjectStatus::Stopping => 5,
        ProjectStatus::Restarting => 4,
        ProjectStatus::Degraded => 3,
        ProjectStatus::Starting => 2,
        ProjectStatus::Ready => 1,
        ProjectStatus::Stopped => 0,
    }
}

/// Project-local state counts and roots owned by an actor.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProjectRuntimeSummary {
    workspace_roots: Vec<PathBuf>,
    configured_language_ids: Vec<String>,
    active_language_ids: Vec<String>,
    open_document_count: usize,
}

impl ProjectRuntimeSummary {
    fn from_translator(translator: &Translator) -> Self {
        Self {
            workspace_roots: translator.workspace_roots().to_vec(),
            configured_language_ids: translator.configured_language_ids(),
            active_language_ids: translator.active_language_ids(),
            open_document_count: translator.open_document_count(),
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

enum ProjectRequest {
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
    ServerLogs {
        limit: usize,
        min_level: Option<String>,
        reply: oneshot::Sender<Result<ServerLogsResult, String>>,
    },
    ServerMessages {
        limit: usize,
        reply: oneshot::Sender<Result<ServerMessagesResult, String>>,
    },
    Notification {
        generation: u64,
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
}

/// Cloneable handle for querying and controlling one project actor.
#[derive(Clone)]
pub struct ProjectHandle {
    sender: mpsc::Sender<ProjectRequest>,
    status: watch::Receiver<ProjectStatus>,
    events: broadcast::Sender<ProjectEvent>,
    event_history: std::sync::Arc<std::sync::Mutex<ProjectEventHistory>>,
}

impl ProjectHandle {
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

    async fn publish_event(&self, event: ProjectEvent) -> Result<(), ProjectActorError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::PublishEvent { event, reply })
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
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ProjectRequest::ApplyEditPlan {
                plan_id,
                project_id,
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
            .send(ProjectRequest::Shutdown { reply })
            .await
            .map_err(|_| ProjectActorError::Closed)?;
        response.await.map_err(|_| ProjectActorError::Cancelled)
    }
}

struct ProjectRuntime {
    translator: Translator,
    edit_plans: EditPlanStore,
    code_actions: CodeActionStore,
    generation: u64,
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
        Self {
            translator,
            edit_plans: EditPlanStore::for_project(),
            code_actions: CodeActionStore::new(),
            generation: 0,
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

    fn take_edit_plan(&mut self, plan_id: &PlanId, project_id: &str) -> Result<EditPlan, String> {
        self.edit_plans
            .take_for_project(plan_id, project_id)
            .map_err(|error| error.to_string())
    }

    async fn apply_edit_plan(
        &mut self,
        plan_id: &PlanId,
        project_id: &str,
        root: &Path,
    ) -> Result<AppliedEditPlan, String> {
        let plan = self
            .edit_plans
            .take_for_project(plan_id, project_id)
            .map_err(|error| error.to_string())?;
        let boundary = WorkspaceBoundary::new(root).map_err(|error| error.to_string())?;
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
        let ApplyReport { committed_files } =
            apply_plan_with_documents(&boundary, &plan, self.translator.document_tracker())
                .map_err(|error| error.to_string())?;
        for (path, version, content) in open_documents {
            self.translator
                .apply_open_document_content(&path, version, content)
                .await
                .map_err(|error| error.to_string())?;
        }
        Ok(AppliedEditPlan {
            plan_id: plan.id().clone(),
            operations: plan.operations().to_vec(),
            unified_diff: plan.unified_diff().to_string(),
            committed_files,
        })
    }

    async fn hover(
        &mut self,
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
        &mut self,
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
        &mut self,
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
            .handle_diagnostics(file_path)
            .await
            .map_err(|error| error.to_string())
    }

    async fn rename(
        &mut self,
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
        &mut self,
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
        &mut self,
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
        &mut self,
        file_path: String,
    ) -> Result<DocumentSymbolsResult, String> {
        self.translator
            .handle_document_symbols(file_path)
            .await
            .map_err(|error| error.to_string())
    }

    async fn format_document(
        &mut self,
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
        &mut self,
        file_path: String,
        tab_size: u32,
        insert_spaces: bool,
    ) -> Result<Option<WorkspaceEdit>, String> {
        self.translator
            .request_format_workspace_edit(file_path, tab_size, insert_spaces)
            .await
            .map_err(|error| error.to_string())
    }

    async fn workspace_symbol(
        &mut self,
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
        &mut self,
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
        &mut self,
        file_path: String,
        line: u32,
        character: u32,
    ) -> Result<CallHierarchyPrepareResult, String> {
        self.translator
            .handle_call_hierarchy_prepare(file_path, line, character)
            .await
            .map_err(|error| error.to_string())
    }

    async fn incoming_calls(
        &mut self,
        item: serde_json::Value,
    ) -> Result<IncomingCallsResult, String> {
        self.translator
            .handle_incoming_calls(item)
            .await
            .map_err(|error| error.to_string())
    }

    async fn outgoing_calls(
        &mut self,
        item: serde_json::Value,
    ) -> Result<OutgoingCallsResult, String> {
        self.translator
            .handle_outgoing_calls(item)
            .await
            .map_err(|error| error.to_string())
    }

    async fn signature_help(
        &mut self,
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
        &mut self,
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
        &mut self,
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
        &mut self,
        file_path: String,
        line: u32,
        character: u32,
    ) -> Result<LocationsResult, String> {
        self.translator
            .handle_type_definition(file_path, line, character)
            .await
            .map_err(|error| error.to_string())
    }

    fn cached_diagnostics(&mut self, file_path: &str) -> Result<DiagnosticsResult, String> {
        self.translator
            .handle_cached_diagnostics(file_path)
            .map_err(|error| error.to_string())
    }

    fn validate_path(&self, file_path: &str) -> Result<(), String> {
        self.translator
            .validate_path(Path::new(file_path))
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    fn server_logs(
        &mut self,
        limit: usize,
        min_level: Option<String>,
    ) -> Result<ServerLogsResult, String> {
        self.translator
            .handle_server_logs(limit, min_level)
            .map_err(|error| error.to_string())
    }

    fn server_messages(&mut self, limit: usize) -> Result<ServerMessagesResult, String> {
        self.translator
            .handle_server_messages(limit)
            .map_err(|error| error.to_string())
    }

    fn notification(&mut self, notification: LspNotification) -> Option<ProjectEvent> {
        match notification {
            LspNotification::PublishDiagnostics(params) => {
                let event = ProjectEvent::DiagnosticsUpdated {
                    uri: params.uri.to_string(),
                    version: params.version,
                    diagnostic_count: params.diagnostics.len(),
                };
                self.translator.notification_cache_mut().store_diagnostics(
                    &params.uri,
                    params.version,
                    params.diagnostics,
                );
                Some(event)
            }
            LspNotification::LogMessage(params) => {
                self.translator
                    .notification_cache_mut()
                    .store_log(params.typ.into(), params.message);
                None
            }
            LspNotification::ShowMessage(params) => {
                self.translator
                    .notification_cache_mut()
                    .store_message(params.typ.into(), params.message);
                None
            }
            LspNotification::Progress { .. }
            | LspNotification::ServerStatus(_)
            | LspNotification::Other { .. } => None,
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
    ) -> Result<Vec<mpsc::Receiver<LspNotification>>, String> {
        self.translator
            .activate_project_with_roots(roots)
            .await
            .map_err(|error| error.to_string())
    }

    async fn add_workspace_root(
        &mut self,
        root: PathBuf,
    ) -> Result<Vec<mpsc::Receiver<LspNotification>>, String> {
        self.translator
            .add_workspace_root(root)
            .await
            .map_err(|error| error.to_string())
    }

    async fn restart(&mut self) -> Result<Vec<mpsc::Receiver<LspNotification>>, String> {
        let roots = self.translator.workspace_roots().to_vec();
        if roots.is_empty() {
            return Ok(Vec::new());
        }
        if self.translator.configured_language_ids().is_empty() {
            return Ok(Vec::new());
        }
        self.shutdown().await?;
        self.translator
            .activate_project_with_roots(roots)
            .await
            .map_err(|error| error.to_string())
    }

    fn summary(&self) -> ProjectRuntimeSummary {
        ProjectRuntimeSummary::from_translator(&self.translator)
    }

    fn open_document_paths(&self) -> Vec<PathBuf> {
        self.translator
            .document_tracker()
            .open_paths()
            .map(PathBuf::from)
            .collect()
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
    spawn_project_actor_with_translator(
        capacity,
        template.translator_for_root(root.as_path().to_path_buf()),
    )
}

/// Spawn an actor with translator state owned exclusively by that actor.
#[must_use]
pub fn spawn_project_actor_with_translator(
    capacity: usize,
    translator: Translator,
) -> ProjectHandle {
    let (sender, receiver) = mpsc::channel(capacity.max(1));
    let actor_sender = sender.clone();
    let (status_tx, status_rx) = watch::channel(ProjectStatus::Starting);
    let (event_tx, _) = broadcast::channel(256);
    let event_sender = event_tx.clone();
    let event_history = std::sync::Arc::new(std::sync::Mutex::new(ProjectEventHistory::new(256)));
    let channels = ProjectActorChannels {
        status_tx,
        event_tx,
        event_history: std::sync::Arc::clone(&event_history),
    };
    let runtime = ProjectRuntime::new(translator);
    tokio::spawn(run_project_actor(
        receiver,
        actor_sender,
        channels,
        ProjectState::new(ProjectStatus::Starting, runtime.summary()),
        runtime,
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
        notification: LspNotification,
    ) {
        if !runtime.owns_generation(generation) {
            return;
        }
        if let Some(event) = runtime.notification(notification) {
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
}

async fn run_project_actor(
    mut receiver: mpsc::Receiver<ProjectRequest>,
    actor_sender: mpsc::Sender<ProjectRequest>,
    channels: ProjectActorChannels,
    mut state: ProjectState,
    mut runtime: ProjectRuntime,
) {
    while let Some(request) = receiver.recv().await {
        if handle_project_request(request, &actor_sender, &channels, &mut state, &mut runtime).await
        {
            break;
        }
    }
}

fn spawn_notification_forwarders(
    notification_receivers: Vec<mpsc::Receiver<LspNotification>>,
    actor_sender: &mpsc::Sender<ProjectRequest>,
    generation: u64,
) {
    for mut receiver in notification_receivers {
        let sender = actor_sender.clone();
        tokio::spawn(async move {
            while let Some(notification) = receiver.recv().await {
                if sender
                    .send(ProjectRequest::Notification {
                        generation,
                        notification,
                    })
                    .await
                    .is_err()
                {
                    break;
                }
            }
            let _ = sender
                .send(ProjectRequest::ServerExited { generation })
                .await;
        });
    }
}

fn mark_project_ready(
    notification_receivers: Vec<mpsc::Receiver<LspNotification>>,
    actor_sender: &mpsc::Sender<ProjectRequest>,
    channels: &ProjectActorChannels,
    state: &mut ProjectState,
    runtime: &ProjectRuntime,
) {
    spawn_notification_forwarders(notification_receivers, actor_sender, runtime.generation());
    state.sync_runtime(runtime);
    channels.publish_status(state, ProjectStatus::Ready);
}

// This exhaustive dispatcher keeps actor state transitions in one place; each
// request arm is intentionally small and independently typed.
#[allow(clippy::too_many_lines)]
#[allow(clippy::large_stack_frames)]
async fn handle_project_request(
    request: ProjectRequest,
    actor_sender: &mpsc::Sender<ProjectRequest>,
    channels: &ProjectActorChannels,
    state: &mut ProjectState,
    runtime: &mut ProjectRuntime,
) -> bool {
    match request {
        ProjectRequest::Query { reply } | ProjectRequest::Refresh { reply } => {
            state.sync_runtime(runtime);
            let _ = reply.send(state.clone());
        }
        ProjectRequest::Activate { root, reply } => {
            runtime.begin_transition();
            state.last_error = None;
            channels.publish_status(state, ProjectStatus::Starting);
            match runtime.translator.activate_project(root).await {
                Ok(notification_receivers) => {
                    mark_project_ready(
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
                    state.last_error = Some(error.to_string());
                    channels.publish_status(state, ProjectStatus::Failed);
                    let _ = reply.send(Err(error.to_string()));
                }
            }
        }
        ProjectRequest::ActivateWorkspaceRoots { roots, reply } => {
            runtime.begin_transition();
            state.last_error = None;
            channels.publish_status(state, ProjectStatus::Starting);
            match runtime.activate_workspace_roots(roots).await {
                Ok(notification_receivers) => {
                    mark_project_ready(
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
                    state.last_error = Some(error.clone());
                    channels.publish_status(state, ProjectStatus::Failed);
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
                    mark_project_ready(
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
                    state.last_error = Some(error.clone());
                    channels.publish_status(state, ProjectStatus::Failed);
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
            reply,
        } => {
            let result = runtime.apply_edit_plan(&plan_id, &project_id, &root).await;
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
        ProjectRequest::Notification {
            generation,
            notification,
        } => {
            channels.publish_notification(runtime, generation, notification);
        }
        ProjectRequest::ServerExited { generation } => {
            if runtime.owns_generation(generation)
                && matches!(state.status, ProjectStatus::Ready | ProjectStatus::Degraded)
            {
                state.last_error = Some("language server exited".to_string());
                channels.publish(ProjectEvent::ServerExited { generation });
                channels.publish_status(state, ProjectStatus::Failed);
            }
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
                    mark_project_ready(
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
                    state.last_error = Some(error);
                    channels.publish_status(state, ProjectStatus::Failed);
                    let _ = reply.send(state.clone());
                }
            }
        }
        ProjectRequest::Fail { message, reply } => {
            state.sync_runtime(runtime);
            state.last_error = Some(message);
            channels.publish_status(state, ProjectStatus::Failed);
            let _ = reply.send(());
        }
        ProjectRequest::Shutdown { reply } => {
            runtime.begin_transition();
            state.sync_runtime(runtime);
            state.last_error = None;
            channels.publish_status(state, ProjectStatus::Stopping);
            if let Err(error) = runtime.shutdown().await {
                state.last_error = Some(error);
            }
            state.sync_runtime(runtime);
            channels.publish_status(state, ProjectStatus::Stopped);
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
    /// The daemon is draining projects and no new registrations are accepted.
    #[error("project registry is shutting down")]
    ShuttingDown,
    /// The project actor could not service the request.
    #[error(transparent)]
    Actor(#[from] ProjectActorError),
}

struct ProjectActorEntry {
    actor: ProjectHandle,
    mutation: MutationGate,
    compatibility_key: Option<ProjectCompatibilityKey>,
    roots: Vec<CanonicalRoot>,
}

impl ProjectActorEntry {
    fn new(
        actor: ProjectHandle,
        mutation: MutationGate,
        compatibility_key: Option<ProjectCompatibilityKey>,
        root: CanonicalRoot,
    ) -> Self {
        Self {
            actor,
            mutation,
            compatibility_key,
            roots: vec![root],
        }
    }
}

struct ProjectEntry {
    identity: ProjectIdentity,
    actors: Vec<ProjectActorEntry>,
}

impl ProjectEntry {
    fn new(
        identity: ProjectIdentity,
        actor: ProjectHandle,
        mutation: MutationGate,
        compatibility_key: Option<ProjectCompatibilityKey>,
    ) -> Self {
        let root = identity.root.clone();
        Self {
            identity,
            actors: vec![ProjectActorEntry::new(
                actor,
                mutation,
                compatibility_key,
                root,
            )],
        }
    }

    fn primary(&self) -> &ProjectActorEntry {
        &self.actors[0]
    }

    fn actor_for_root(&self, root: &Path) -> Option<&ProjectActorEntry> {
        self.actors.iter().find(|actor| {
            actor
                .roots
                .iter()
                .any(|candidate| candidate.as_path() == root)
        })
    }
}

type MutationGate = std::sync::Arc<Mutex<()>>;

#[derive(Debug, Default)]
struct RegistryLifecycle {
    shutting_down: AtomicBool,
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
}

/// Process-wide registry of project identities and their actor handles.
#[derive(Clone)]
pub struct ProjectRegistry {
    projects: std::sync::Arc<RwLock<HashMap<ProjectId, ProjectEntry>>>,
    actor_capacity: usize,
    translator_template: Option<std::sync::Arc<TranslatorTemplate>>,
    persistence: Option<std::sync::Arc<ProjectRegistrationStore>>,
    lifecycle: std::sync::Arc<RegistryLifecycle>,
    shutdown_timeout: Duration,
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
    /// Projects draining before shutdown.
    pub stopping: usize,
    /// Stopped projects still retained by the registry.
    pub stopped: usize,
    /// Failed projects.
    pub failed: usize,
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
    fn spawn_actor(&self, root: &CanonicalRoot) -> ProjectHandle {
        self.translator_template.as_deref().map_or_else(
            || spawn_project_actor_for_root(self.actor_capacity, root),
            |template| {
                spawn_project_actor_for_root_with_template(self.actor_capacity, root, template)
            },
        )
    }

    fn with_template(
        actor_capacity: usize,
        translator_template: Option<TranslatorTemplate>,
    ) -> Self {
        Self {
            projects: std::sync::Arc::new(RwLock::new(HashMap::new())),
            actor_capacity: actor_capacity.max(1),
            translator_template: translator_template.map(std::sync::Arc::new),
            persistence: None,
            lifecycle: std::sync::Arc::new(RegistryLifecycle::default()),
            shutdown_timeout: DEFAULT_PROJECT_SHUTDOWN_TIMEOUT,
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
        let projects = self
            .list()
            .await
            .iter()
            .map(PersistedProject::from_identity)
            .collect::<Vec<_>>();
        save_persisted_state(store, projects).await
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
        let state = load_persisted_state(store).await?;
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
            self.add(identity).await?;
            for additional_root in &persisted.additional_roots {
                let Ok(additional_root) = CanonicalRoot::new(additional_root) else {
                    continue;
                };
                let Ok(Some(repository)) =
                    GitRepositoryIdentity::discover(additional_root.as_path())
                else {
                    continue;
                };
                self.add(
                    ProjectIdentity::new(id.clone(), additional_root)
                        .with_repository_identity(repository),
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
        let compatibility_key = rust_project_compatibility_key(identity.root.as_path());
        let mut projects = self.projects.write().await;
        self.lifecycle.ensure_accepting()?;
        if let Some(existing) = projects.get(identity.id()) {
            if let Some(actor) = existing.actor_for_root(identity.root().as_path()) {
                return Ok(actor.actor.clone());
            }
            let compatible = existing
                .actors
                .iter()
                .find(|actor| {
                    actor.compatibility_key == compatibility_key
                        && compatibility_key.is_some()
                        && existing.identity.repository_identity() == identity.repository_identity()
                })
                .map(|actor| (actor.actor.clone(), actor.mutation.clone()));
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
            let actor = self.spawn_actor(identity.root());
            let mutation = std::sync::Arc::new(Mutex::new(()));
            drop(projects);
            let mut projects = self.projects.write().await;
            if let Some(existing) = projects.get_mut(identity.id()) {
                existing.identity.add_root(identity.root.clone());
                existing.actors.push(ProjectActorEntry::new(
                    actor.clone(),
                    mutation,
                    compatibility_key,
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
        let actor = self.spawn_actor(&primary_root);
        let mutation = std::sync::Arc::new(Mutex::new(()));
        projects.insert(
            identity.id().clone(),
            ProjectEntry::new(identity, actor.clone(), mutation, compatibility_key),
        );
        drop(projects);
        self.persist().await?;
        Ok(actor)
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
            for actor in &entry.actors {
                let status = *actor.actor.status().borrow();
                match status {
                    ProjectStatus::Starting => counts.starting += 1,
                    ProjectStatus::Ready => counts.ready += 1,
                    ProjectStatus::Degraded => counts.degraded += 1,
                    ProjectStatus::Restarting => counts.restarting += 1,
                    ProjectStatus::Stopping => counts.stopping += 1,
                    ProjectStatus::Stopped => counts.stopped += 1,
                    ProjectStatus::Failed => counts.failed += 1,
                }
            }
        }
        drop(projects);
        counts
    }

    /// Gracefully stop every registered project actor once.
    ///
    /// Requests already queued on an actor are processed before its shutdown
    /// request, preserving edit commit boundaries without holding the registry
    /// lock across the await.
    pub async fn shutdown_all(&self) -> ProjectShutdownReport {
        self.lifecycle.begin_shutdown();
        let _mutation_guards = self.lock_project_mutations().await;
        let entries: Vec<_> = self
            .projects
            .read()
            .await
            .values()
            .flat_map(|entry| {
                entry
                    .actors
                    .iter()
                    .map(move |actor| (entry.identity.id().clone(), actor.actor.clone()))
            })
            .collect();

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
        let entries: Vec<_> = self
            .projects
            .read()
            .await
            .values()
            .flat_map(|entry| {
                entry
                    .actors
                    .iter()
                    .map(move |actor| (entry.identity.id().clone(), actor.actor.clone()))
            })
            .collect();
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

    /// Remove a project and shut down its actor when no linked project remains.
    ///
    /// # Errors
    ///
    /// Returns an error when the project is not registered or its actor cannot shut down.
    pub async fn remove(&self, id: ProjectId) -> Result<(), ProjectRegistryError> {
        let (actors, mutations, root) = self
            .projects
            .read()
            .await
            .get(&id)
            .map(|entry| {
                (
                    entry
                        .actors
                        .iter()
                        .map(|actor| actor.actor.clone())
                        .collect::<Vec<_>>(),
                    entry
                        .actors
                        .iter()
                        .map(|actor| actor.mutation.clone())
                        .collect::<Vec<_>>(),
                    entry.identity.root().as_path().to_path_buf(),
                )
            })
            .ok_or_else(|| ProjectRegistryError::ProjectNotFound(id.clone()))?;
        let _mutation_guards = self.lock_mutation_gates(mutations).await;
        self.projects
            .write()
            .await
            .remove(&id)
            .ok_or_else(|| ProjectRegistryError::ProjectNotFound(id.clone()))?;
        self.persist().await?;
        for actor in actors {
            actor
                .publish_event(ProjectEvent::ProjectRemoved {
                    project_id: id.clone(),
                    root: root.clone(),
                })
                .await
                .map_err(ProjectRegistryError::from)?;
            actor.shutdown().await.map_err(ProjectRegistryError::from)?;
        }
        Ok(())
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
        let (identity, actor, mutation) = self.entry(id).await?;
        let _mutation = mutation.lock().await;
        actor
            .apply_edit_plan(
                plan_id,
                id.as_str().to_string(),
                identity.root().as_path().to_path_buf(),
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
            && project
                .actors
                .iter()
                .any(|actor| actor.compatibility_key == Some(compatibility_key))
    })
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
    async fn project_actor_publishes_server_exit_events_before_failure_status() {
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
                status: ProjectStatus::Failed,
                last_error: Some("language server exited".to_string()),
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
                notification,
            })
            .await
            .unwrap();

        let result = actor.server_logs(10, None).await.unwrap();
        assert_eq!(result.logs.len(), 1);
        assert_eq!(result.logs[0].message, "project log");
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
        assert_eq!(state.status(), ProjectStatus::Failed);
        assert_eq!(state.last_error(), Some("language server exited"));
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
        for root in [repository.path(), worktree.path()] {
            fs::write(
                root.join("rust-toolchain.toml"),
                "[toolchain]\nchannel = \"stable\"\n",
            )
            .unwrap();
            fs::write(root.join("Cargo.toml"), "[package]\nname = \"fixture\"\n").unwrap();
        }
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
                },
                PersistedProject {
                    project_id: "missing".to_string(),
                    root: root.path().join("gone"),
                    additional_roots: Vec::new(),
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
        );
        entry.identity.add_root(secondary_root.clone());
        entry.actors.push(ProjectActorEntry::new(
            secondary,
            std::sync::Arc::new(Mutex::new(())),
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
            sender,
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
                    compatibility_key: None,
                    roots: vec![CanonicalRoot::new(root.path()).unwrap()],
                }],
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
        assert_eq!(actor.query().await.unwrap().workspace_roots().len(), 2);
        let file = worktree.path().join("src.rs");
        fs::write(&file, "fn main() {}\n").unwrap();
        let (resolved_id, resolved_actor) = registry.project_for_path(&file).await.unwrap();
        assert_eq!(resolved_id, project_id);
        assert!(resolved_actor.sender.same_channel(&actor.sender));
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
