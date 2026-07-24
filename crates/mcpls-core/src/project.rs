//! Project identity and canonical path routing primitives.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use tokio::sync::{RwLock, mpsc, oneshot, watch};

use crate::bridge::{
    CallHierarchyPrepareResult, CodeActionsResult, CompletionsResult, DefinitionResult,
    DiagnosticsResult, DocumentSymbolsResult, FormatDocumentResult, HoverResult, InlayHintsResult,
    LocationsResult, ReferencesResult, RenameResult, SignatureHelpResult, Translator,
    TranslatorTemplate, WorkspaceSymbolResult,
};

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
        let canonical = match canonicalize(path) {
            Ok(canonical) => canonical,
            Err(error) => {
                if let Some(project) = self.projects.iter().find(|project| {
                    !project.root.as_path().exists() && path.starts_with(project.root.as_path())
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
            .filter(|project| {
                project.root.as_path().exists() && canonical.starts_with(project.root.as_path())
            })
            .max_by_key(|project| project.root.as_path().components().count())
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
                if project.root.as_path().exists() && canonical.starts_with(project.root.as_path())
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
    PrepareCallHierarchy {
        file_path: String,
        line: u32,
        character: u32,
        reply: oneshot::Sender<Result<CallHierarchyPrepareResult, String>>,
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
}

impl ProjectHandle {
    /// Subscribe to lifecycle changes for this project.
    #[must_use]
    pub fn status(&self) -> watch::Receiver<ProjectStatus> {
        self.status.clone()
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
}

impl ProjectRuntime {
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

    fn summary(&self) -> ProjectRuntimeSummary {
        ProjectRuntimeSummary::from_translator(&self.translator)
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
    let (status_tx, status_rx) = watch::channel(ProjectStatus::Starting);
    let runtime = ProjectRuntime { translator };
    tokio::spawn(run_project_actor(
        receiver,
        status_tx,
        ProjectState::new(ProjectStatus::Starting, runtime.summary()),
        runtime,
    ));
    ProjectHandle {
        sender,
        status: status_rx,
    }
}

async fn run_project_actor(
    mut receiver: mpsc::Receiver<ProjectRequest>,
    status_tx: watch::Sender<ProjectStatus>,
    mut state: ProjectState,
    mut runtime: ProjectRuntime,
) {
    while let Some(request) = receiver.recv().await {
        if handle_project_request(request, &status_tx, &mut state, &mut runtime).await {
            break;
        }
    }
}

// This exhaustive dispatcher keeps actor state transitions in one place; each
// request arm is intentionally small and independently typed.
#[allow(clippy::too_many_lines)]
async fn handle_project_request(
    request: ProjectRequest,
    status_tx: &watch::Sender<ProjectStatus>,
    state: &mut ProjectState,
    runtime: &mut ProjectRuntime,
) -> bool {
    match request {
        ProjectRequest::Query { reply } | ProjectRequest::Refresh { reply } => {
            state.sync_runtime(runtime);
            let _ = reply.send(state.clone());
        }
        ProjectRequest::Activate { root, reply } => {
            state.status = ProjectStatus::Starting;
            state.last_error = None;
            let _ = status_tx.send(ProjectStatus::Starting);
            match runtime.translator.activate_project(root).await {
                Ok(()) => {
                    state.sync_runtime(runtime);
                    state.status = ProjectStatus::Ready;
                    let _ = status_tx.send(ProjectStatus::Ready);
                    let _ = reply.send(Ok(state.clone()));
                }
                Err(error) => {
                    state.sync_runtime(runtime);
                    state.status = ProjectStatus::Failed;
                    state.last_error = Some(error.to_string());
                    let _ = status_tx.send(ProjectStatus::Failed);
                    let _ = reply.send(Err(error.to_string()));
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
        ProjectRequest::SetStatus { status, reply } => {
            state.sync_runtime(runtime);
            state.status = status;
            state.last_error = None;
            let _ = status_tx.send(status);
            let _ = reply.send(());
        }
        ProjectRequest::Restart { reply } => {
            state.sync_runtime(runtime);
            state.status = ProjectStatus::Restarting;
            state.last_error = None;
            let _ = status_tx.send(ProjectStatus::Restarting);
            state.status = ProjectStatus::Ready;
            let _ = status_tx.send(ProjectStatus::Ready);
            let _ = reply.send(state.clone());
        }
        ProjectRequest::Fail { message, reply } => {
            state.sync_runtime(runtime);
            state.status = ProjectStatus::Failed;
            state.last_error = Some(message);
            let _ = status_tx.send(ProjectStatus::Failed);
            let _ = reply.send(());
        }
        ProjectRequest::Shutdown { reply } => {
            state.sync_runtime(runtime);
            state.status = ProjectStatus::Stopping;
            state.last_error = None;
            let _ = status_tx.send(ProjectStatus::Stopping);
            state.status = ProjectStatus::Stopped;
            let _ = status_tx.send(ProjectStatus::Stopped);
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
    /// No project with this stable ID is registered.
    #[error("project is not registered: {0}")]
    ProjectNotFound(ProjectId),
    /// The project actor could not service the request.
    #[error(transparent)]
    Actor(#[from] ProjectActorError),
}

struct ProjectEntry {
    identity: ProjectIdentity,
    actor: ProjectHandle,
}

/// Process-wide registry of project identities and their actor handles.
#[derive(Clone)]
pub struct ProjectRegistry {
    projects: std::sync::Arc<RwLock<HashMap<ProjectId, ProjectEntry>>>,
    actor_capacity: usize,
    translator_template: Option<std::sync::Arc<TranslatorTemplate>>,
}

impl ProjectRegistry {
    /// Create an empty registry with a bounded actor queue capacity.
    #[must_use]
    pub fn new(actor_capacity: usize) -> Self {
        Self {
            projects: std::sync::Arc::new(RwLock::new(HashMap::new())),
            actor_capacity: actor_capacity.max(1),
            translator_template: None,
        }
    }

    /// Create a registry whose actors inherit only the daemon translator's configuration.
    #[must_use]
    pub fn with_translator_template(actor_capacity: usize, template: TranslatorTemplate) -> Self {
        Self {
            projects: std::sync::Arc::new(RwLock::new(HashMap::new())),
            actor_capacity: actor_capacity.max(1),
            translator_template: Some(std::sync::Arc::new(template)),
        }
    }

    /// Add a project, returning its existing actor when the ID is already present.
    ///
    /// # Errors
    ///
    /// Returns [`ProjectRegistryError::DuplicateRoot`] when another project owns the root.
    pub async fn add(
        &self,
        identity: ProjectIdentity,
    ) -> Result<ProjectHandle, ProjectRegistryError> {
        let mut projects = self.projects.write().await;
        if let Some(existing) = projects.get(identity.id()) {
            if existing.identity.root() != identity.root() {
                return Err(ProjectRegistryError::ConflictingProject {
                    id: identity.id().clone(),
                    existing_root: existing.identity.root().as_path().to_path_buf(),
                    requested_root: identity.root().as_path().to_path_buf(),
                });
            }
            let actor = existing.actor.clone();
            drop(projects);
            return Ok(actor);
        }
        if projects
            .values()
            .any(|project| project.identity.root() == identity.root())
        {
            return Err(ProjectRegistryError::DuplicateRoot(
                identity.root().as_path().to_path_buf(),
            ));
        }

        let actor = self.translator_template.as_deref().map_or_else(
            || spawn_project_actor_for_root(self.actor_capacity, identity.root()),
            |template| {
                spawn_project_actor_for_root_with_template(
                    self.actor_capacity,
                    identity.root(),
                    template,
                )
            },
        );
        projects.insert(
            identity.id().clone(),
            ProjectEntry {
                identity,
                actor: actor.clone(),
            },
        );
        drop(projects);
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

    /// Remove a project and ask its actor to shut down after releasing the registry lock.
    ///
    /// # Errors
    ///
    /// Returns an error when the project is not registered or its actor cannot shut down.
    pub async fn remove(&self, id: ProjectId) -> Result<(), ProjectRegistryError> {
        let actor = self
            .projects
            .write()
            .await
            .remove(&id)
            .ok_or(ProjectRegistryError::ProjectNotFound(id))?
            .actor;
        actor.shutdown().await.map_err(ProjectRegistryError::from)
    }

    /// Query a project's actor state without holding the registry lock during the await.
    ///
    /// # Errors
    ///
    /// Returns an error when the project is not registered or its actor is unavailable.
    pub async fn status(&self, id: &ProjectId) -> Result<ProjectState, ProjectRegistryError> {
        self.actor(id)
            .await?
            .query()
            .await
            .map_err(ProjectRegistryError::from)
    }

    /// Activate a registered project's actor-owned language servers.
    ///
    /// # Errors
    ///
    /// Returns an error when the project is not registered or activation fails.
    pub async fn activate(&self, id: &ProjectId) -> Result<ProjectState, ProjectRegistryError> {
        let identity = self.identity(id).await?;
        self.actor(id)
            .await?
            .activate(identity.root().as_path().to_path_buf())
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
        self.actor(id)
            .await?
            .restart()
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
        let canonical = canonicalize(path.as_ref())?;
        self.projects
            .read()
            .await
            .values()
            .filter(|project| canonical.starts_with(project.identity.root().as_path()))
            .max_by_key(|project| project.identity.root().as_path().components().count())
            .map(|project| project.actor.clone())
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
            .map(|project| project.actor.clone())
            .ok_or_else(|| ProjectRegistryError::ProjectNotFound(id.clone()))
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
        assert!(registry.list().await.is_empty());
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
