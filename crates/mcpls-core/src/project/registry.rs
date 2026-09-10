//! Shared project registry, identity grouping, persistence, and shutdown.

#![allow(clippy::redundant_pub_crate)]

#[allow(clippy::wildcard_imports)]
use super::*;
#[allow(clippy::wildcard_imports)]
use super::{actor::*, identity::*, runtime::*, state::*};

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
    /// A linked worktree must use its logical project's stable ID.
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
pub(super) enum ProjectCompatibility {
    Deferred,
    Resolved(Option<ProjectCompatibilityKey>),
}

pub(super) struct ProjectActorEntry {
    pub(super) actor: ProjectHandle,
    pub(super) mutation: MutationGate,
    pub(super) compatibility: ProjectCompatibility,
    pub(super) translator_template: Option<std::sync::Arc<TranslatorTemplate>>,
    pub(super) roots: Vec<CanonicalRoot>,
}

pub(super) struct CargoFeatureActorSnapshot {
    mutation: MutationGate,
    roots: Vec<CanonicalRoot>,
    translator_template: Option<std::sync::Arc<TranslatorTemplate>>,
}

impl ProjectActorEntry {
    pub(super) fn new(
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

pub(super) struct ProjectEntry {
    pub(super) identity: ProjectIdentity,
    pub(super) actors: Vec<ProjectActorEntry>,
    pub(super) config: Option<ProjectConfig>,
}

pub(super) struct ProjectRemovalSnapshot {
    actors: Vec<ProjectHandle>,
    mutations: Vec<MutationGate>,
    root: PathBuf,
}

pub(super) const RETAINED_PROJECT_HISTORY_CAPACITY: usize = 16;
pub(super) const RETAINED_LOG_CAPACITY: usize = 100;
pub(super) const RETAINED_MESSAGE_CAPACITY: usize = 50;

/// Bounded, in-memory history retained after a project is removed.
///
/// Retention is deliberately process-local and limited to the most recent 16
/// removed projects. It is not persisted and is cleared when a project ID is
/// registered again.
#[derive(Debug, Default)]
pub(super) struct RetainedProjectHistories {
    entries: HashMap<ProjectId, RetainedProjectHistory>,
    order: VecDeque<ProjectId>,
}

#[derive(Debug, Clone, Default)]
pub(super) struct RetainedProjectHistory {
    logs: Vec<LogEntry>,
    messages: Vec<ServerMessage>,
    capabilities: Vec<ProjectServerCapability>,
}

impl RetainedProjectHistory {
    pub(super) fn page_bounds<T: Serialize>(
        items: &[T],
        limit: usize,
        cursor: Option<&str>,
        kind: &str,
    ) -> Result<(std::ops::Range<usize>, String, Option<String>), ProjectActorError> {
        let snapshot_identity = format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(items).unwrap_or_default())
        );
        let start = match cursor {
            Some(cursor) => {
                let (identity, offset) = cursor.split_once(':').ok_or_else(|| {
                    ProjectActorError::Operation(format!("invalid {kind} cursor: {cursor}"))
                })?;
                if identity != snapshot_identity {
                    return Err(ProjectActorError::Operation(format!(
                        "{kind} cursor belongs to a different snapshot"
                    )));
                }
                offset.parse::<usize>().map_err(|_| {
                    ProjectActorError::Operation(format!("invalid {kind} cursor: {cursor}"))
                })?
            }
            None => 0,
        };
        if cursor.is_some() && start >= items.len() {
            return Err(ProjectActorError::Operation(format!(
                "{kind} cursor is outside the retained snapshot: {start}"
            )));
        }
        let end = start.saturating_add(limit).min(items.len());
        let next_cursor =
            (limit > 0 && end < items.len()).then(|| format!("{snapshot_identity}:{end}"));
        Ok((start..end, snapshot_identity, next_cursor))
    }

    pub(super) fn server_logs_page(
        &self,
        limit: usize,
        min_level: Option<&str>,
        cursor: Option<&str>,
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
        let logs: Vec<_> = self
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
            .cloned()
            .collect();
        let (page, snapshot_identity, next_cursor) =
            Self::page_bounds(&logs, limit, cursor, "server_logs")?;
        let page_end = page.end;
        let total = logs.len();
        let logs = if limit > 0 {
            logs[page].to_vec()
        } else {
            Vec::new()
        };
        Ok(ServerLogsResult {
            returned: logs.len(),
            remaining: total.saturating_sub(page_end),
            total,
            snapshot_identity,
            next_cursor,
            logs,
        })
    }

    pub(super) fn server_messages_page(
        &self,
        limit: usize,
        cursor: Option<&str>,
    ) -> Result<ServerMessagesResult, ProjectActorError> {
        let (page, snapshot_identity, next_cursor) =
            Self::page_bounds(&self.messages, limit, cursor, "server_messages")?;
        let page_end = page.end;
        let messages: Vec<_> = if limit > 0 {
            self.messages
                .iter()
                .skip(page.start)
                .take(page.end - page.start)
                .cloned()
                .collect()
        } else {
            Vec::new()
        };
        Ok(ServerMessagesResult {
            returned: messages.len(),
            remaining: self.messages.len().saturating_sub(page_end),
            total: self.messages.len(),
            snapshot_identity,
            next_cursor,
            messages,
        })
    }
}

impl RetainedProjectHistories {
    pub(super) fn insert(&mut self, id: ProjectId, history: RetainedProjectHistory) {
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

    pub(super) fn remove(&mut self, id: &ProjectId) {
        self.entries.remove(id);
        self.order.retain(|existing| existing != id);
    }
}

impl ProjectRemovalSnapshot {
    pub(super) fn reject_new_work(&self) {
        reject_new_actor_work(&self.actors);
    }

    pub(super) fn accept_new_work(&self) {
        for actor in &self.actors {
            actor.accept_new_work();
        }
    }

    pub(super) async fn shutdown(
        &self,
        project_id: &ProjectId,
    ) -> Result<(), ProjectRegistryError> {
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

    pub(super) async fn capture_history(&self) -> RetainedProjectHistory {
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
    pub(super) fn new(
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

    pub(super) fn primary(&self) -> &ProjectActorEntry {
        &self.actors[0]
    }

    pub(super) fn primary_mut(&mut self) -> &mut ProjectActorEntry {
        &mut self.actors[0]
    }

    pub(super) fn removal_snapshot(&self) -> ProjectRemovalSnapshot {
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

    pub(super) fn actor_for_root(&self, root: &Path) -> Option<&ProjectActorEntry> {
        self.actors.iter().find(|actor| {
            actor
                .roots
                .iter()
                .any(|candidate| candidate.as_path() == root)
        })
    }

    pub(super) fn compatible_actor(
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

    pub(super) fn status(&self) -> ProjectStatus {
        aggregate_statuses(
            self.actors
                .iter()
                .map(|actor| *actor.actor.status().borrow()),
        )
    }

    pub(super) fn status_summary(&self) -> ProjectStatusSummary {
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

    pub(super) fn queue_pressure(&self) -> ProjectQueuePressure {
        self.actors
            .iter()
            .map(|actor| actor.actor.queue_pressure())
            .fold(ProjectQueuePressure::default(), ProjectQueuePressure::add)
    }
}

pub(super) type MutationGate = std::sync::Arc<Mutex<()>>;

#[derive(Debug, Default)]
pub(super) struct RegistryLifecycle {
    shutting_down: AtomicBool,
    removing: Mutex<HashSet<ProjectId>>,
}

pub(super) const DEFAULT_PROJECT_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30);

impl RegistryLifecycle {
    pub(super) fn begin_shutdown(&self) {
        self.shutting_down.store(true, Ordering::Release);
    }

    pub(super) fn ensure_accepting(&self) -> Result<(), ProjectRegistryError> {
        if self.shutting_down.load(Ordering::Acquire) {
            Err(ProjectRegistryError::ShuttingDown)
        } else {
            Ok(())
        }
    }

    pub(super) async fn ensure_project_available(
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

    pub(super) async fn begin_removal(
        &self,
        project_id: &ProjectId,
    ) -> Result<(), ProjectRegistryError> {
        let mut removing = self.removing.lock().await;
        if removing.insert(project_id.clone()) {
            Ok(())
        } else {
            Err(ProjectRegistryError::ProjectRemoving(project_id.clone()))
        }
    }

    pub(super) async fn end_removal(&self, project_id: &ProjectId) {
        self.removing.lock().await.remove(project_id);
    }
}

pub(super) type EditInFlight = std::sync::Arc<
    Mutex<HashMap<(String, String), watch::Sender<Option<Result<ApplyEditPlanOutcome, String>>>>>,
>;

/// Process-wide registry of project identities and their actor handles.
#[derive(Clone)]
pub struct ProjectRegistry {
    pub(super) projects: std::sync::Arc<RwLock<HashMap<ProjectId, ProjectEntry>>>,
    retained_history: std::sync::Arc<RwLock<RetainedProjectHistories>>,
    actor_capacity: usize,
    translator_template: Option<std::sync::Arc<TranslatorTemplate>>,
    persistence: Option<std::sync::Arc<ProjectRegistrationStore>>,
    persistence_error: std::sync::Arc<RwLock<Option<String>>>,
    lifecycle: std::sync::Arc<RegistryLifecycle>,
    shutdown_timeout: Duration,
    rust_residency: RustResidencyController,
    next_rust_group_id: std::sync::Arc<AtomicU64>,
    pub(super) deferred_results: std::sync::Arc<std::sync::Mutex<DeferredResultStore>>,
    pub(super) edit_coordinator: std::sync::Arc<EditCoordinator>,
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
    pub(super) fn from_server(group_id: usize, capability: ServerCapability) -> Self {
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
    pub(super) fn record_actor_result(
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

    pub(super) fn record_actor_timeout(&mut self, project_ids: Vec<ProjectId>, timeout: Duration) {
        self.failed.extend(
            project_ids
                .into_iter()
                .map(|project_id| ProjectShutdownFailure {
                    project_id,
                    error: format!("shutdown timed out after {timeout:?}"),
                }),
        );
    }

    pub(super) fn sort(&mut self) {
        self.stopped.sort();
        self.failed
            .sort_by(|left, right| left.project_id.cmp(&right.project_id));
    }
}

pub(super) enum ShutdownAttempt {
    Completed(Result<(), ProjectActorError>),
    TimedOut,
}

pub(super) async fn shutdown_actor_with_timeout(
    actor: ProjectHandle,
    timeout: Duration,
) -> ShutdownAttempt {
    tokio::time::timeout(timeout, actor.shutdown())
        .await
        .map_or(ShutdownAttempt::TimedOut, ShutdownAttempt::Completed)
}

pub(super) async fn add_actor_roots(
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

pub(super) async fn shutdown_project_actors(actors: &[ProjectActorEntry]) {
    for actor in actors {
        let _ = actor.actor.shutdown().await;
    }
}

impl ProjectRegistry {
    pub(super) fn spawn_actor(
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

    pub(super) fn with_template(
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

    pub(super) async fn persist(&self) -> Result<(), ProjectRegistryError> {
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

    pub(super) async fn record_persistence_error(&self, error: Option<String>) {
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

    pub(super) async fn add_restored_with_config(
        &self,
        identity: ProjectIdentity,
        config: Option<ProjectConfig>,
    ) -> Result<ProjectHandle, ProjectRegistryError> {
        self.add_configured_registration(identity, config, true)
            .await
    }

    pub(super) async fn add_configured_registration(
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
    pub(super) async fn add_registration(
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
            // An unknown compatibility key is not a conflict: keep the root
            // in the logical repository while isolating it in its own actor.
            if existing.identity.repository_identity().is_none()
                || identity.repository_identity().is_none()
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

        if let Some(existing) = repository_project(&projects, &identity) {
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

    pub(super) async fn resolve_deferred_compatibility_keys(
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

    pub(super) async fn lock_project_mutations(&self) -> Vec<tokio::sync::OwnedMutexGuard<()>> {
        let mutations = {
            let projects = self.projects.read().await;
            unique_mutation_gates(&projects)
        };
        self.lock_mutation_gates(mutations).await
    }

    pub(super) async fn registered_actor_entries(&self) -> Vec<(ProjectId, ProjectHandle)> {
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

    pub(super) async fn lock_mutation_gates(
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

    pub(super) async fn begin_project_removal(
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

    pub(super) async fn abort_project_removal(
        &self,
        id: &ProjectId,
        removal: &ProjectRemovalSnapshot,
        error: ProjectRegistryError,
    ) -> Result<(), ProjectRegistryError> {
        removal.accept_new_work();
        self.lifecycle.end_removal(id).await;
        Err(error)
    }

    pub(super) async fn retain_history(&self, id: ProjectId, history: RetainedProjectHistory) {
        self.retained_history.write().await.insert(id, history);
    }

    pub(super) async fn retained_history_for(
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
        self.server_logs_page(id, limit, min_level, None).await
    }

    /// Return one snapshot-bound page of logs from a project's primary actor.
    ///
    /// # Errors
    ///
    /// Returns an error if the project is not registered, the actor closes, or
    /// the requested log filter is invalid.
    pub async fn server_logs_page(
        &self,
        id: &ProjectId,
        limit: usize,
        min_level: Option<String>,
        cursor: Option<String>,
    ) -> Result<ServerLogsResult, ProjectRegistryError> {
        match self.actor(id).await {
            Ok(actor) => actor
                .server_logs_page(limit, min_level, cursor)
                .await
                .map_err(ProjectRegistryError::from),
            Err(ProjectRegistryError::ProjectNotFound(_)) => {
                let mut result = self
                    .retained_history_for(id)
                    .await?
                    .server_logs_page(limit, min_level.as_deref(), cursor.as_deref())
                    .map_err(ProjectRegistryError::from)?;
                defer_notification_messages_for_scope(
                    &mut result.logs,
                    "diagnostic_log_message",
                    &self.deferred_results,
                    id.as_str(),
                )
                .map_err(ProjectActorError::Operation)
                .map_err(ProjectRegistryError::from)?;
                bound_server_logs_result(&mut result, cursor.as_deref())
                    .map_err(ProjectActorError::Operation)
                    .map_err(ProjectRegistryError::from)?;
                Ok(result)
            }
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
        self.server_messages_page(id, limit, None).await
    }

    /// Return one snapshot-bound page of messages from a project's primary actor.
    ///
    /// # Errors
    ///
    /// Returns an error if the project is not registered or its actor closes.
    pub async fn server_messages_page(
        &self,
        id: &ProjectId,
        limit: usize,
        cursor: Option<String>,
    ) -> Result<ServerMessagesResult, ProjectRegistryError> {
        match self.actor(id).await {
            Ok(actor) => actor
                .server_messages_page(limit, cursor)
                .await
                .map_err(ProjectRegistryError::from),
            Err(ProjectRegistryError::ProjectNotFound(_)) => {
                let mut result = self
                    .retained_history_for(id)
                    .await?
                    .server_messages_page(limit, cursor.as_deref())?;
                defer_notification_messages_for_scope(
                    &mut result.messages,
                    "server_message",
                    &self.deferred_results,
                    id.as_str(),
                )
                .map_err(ProjectActorError::Operation)
                .map_err(ProjectRegistryError::from)?;
                bound_server_messages_result(&mut result, cursor.as_deref())
                    .map_err(ProjectActorError::Operation)
                    .map_err(ProjectRegistryError::from)?;
                Ok(result)
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
        page_token: Option<String>,
    ) -> Result<CodeActionsResult, ProjectRegistryError> {
        let (actor, _, _) = self.entry_for_path(id, Path::new(&file_path)).await?;
        actor
            .code_action_list(
                file_path,
                start_line,
                start_character,
                end_line,
                end_character,
                kind_filter,
                page_token,
            )
            .await
            .map_err(ProjectRegistryError::from)
    }

    pub(crate) async fn path_rename_preview(
        &self,
        id: &ProjectId,
        request: PathRenameRequest,
    ) -> Result<PathRenamePreview, ProjectRegistryError> {
        let (actor, mutation, root) = self
            .entry_for_path(id, Path::new(&request.old_path))
            .await?;
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
        let (actor, mutation, root) = self.locate_code_action(id, action_id.clone()).await?;
        let _mutation = mutation.lock().await;
        actor
            .preview_code_action(action_id, id.as_str().to_string(), encoding, root)
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

    pub(super) async fn replace_project_actors_transactionally(
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

    pub(super) async fn cargo_feature_snapshot(
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

    pub(super) async fn build_cargo_feature_replacements(
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

    pub(super) async fn swap_project_actors(
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

    pub(super) async fn rollback_project_actors(
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
        let (_, _, summary) = self.locate_edit_plan(id, plan_id).await?;
        summary.ok_or_else(|| {
            ProjectRegistryError::Actor(ProjectActorError::Operation(
                "edit plan not found".to_owned(),
            ))
        })
    }

    /// Find a pending plan or an applied receipt across all actor groups of a
    /// logical project. Linked worktrees may deliberately use separate actors
    /// when their language-server compatibility is unknown.
    async fn locate_edit_plan(
        &self,
        id: &ProjectId,
        plan_id: PlanId,
    ) -> Result<
        (
            ProjectHandle,
            PathBuf,
            Option<crate::edit_plan::EditPlanApprovalSummary>,
        ),
        ProjectRegistryError,
    > {
        let candidates = self
            .projects
            .read()
            .await
            .get(id)
            .map(|project| {
                project
                    .actors
                    .iter()
                    .filter_map(|entry| {
                        entry
                            .roots
                            .first()
                            .map(|root| (entry.actor.clone(), root.as_path().to_path_buf()))
                    })
                    .collect::<Vec<_>>()
            })
            .ok_or_else(|| ProjectRegistryError::ProjectNotFound(id.clone()))?;
        for (actor, root) in candidates {
            match actor
                .inspect_edit_plan(plan_id.clone(), id.as_str().to_owned())
                .await
            {
                Ok(summary) => return Ok((actor, root, Some(summary))),
                Err(error) if error.to_string().contains("edit plan not found") => {
                    if actor
                        .has_edit_plan_receipt_or_conflict(plan_id.clone())
                        .await
                        .map_err(ProjectRegistryError::from)?
                    {
                        return Ok((actor, root, None));
                    }
                }
                Err(error) => return Err(ProjectRegistryError::from(error)),
            }
        }
        Err(ProjectRegistryError::Actor(ProjectActorError::Operation(
            "edit plan not found".to_owned(),
        )))
    }

    /// Resolve the actor that owns a plan for session-scoped edit resources.
    pub(crate) async fn actor_for_edit_plan(
        &self,
        id: &ProjectId,
        plan_id: PlanId,
    ) -> Result<ProjectHandle, ProjectRegistryError> {
        self.locate_edit_plan(id, plan_id)
            .await
            .map(|(actor, _, _)| actor)
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

    pub(super) async fn wait_for_in_flight(
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

    pub(super) async fn apply_edit_plan_leader(
        &self,
        id: &ProjectId,
        plan_id: PlanId,
        session_id: Option<String>,
        principal: Option<String>,
        wait: Duration,
    ) -> Result<ApplyEditPlanOutcome, ProjectRegistryError> {
        let (actor, root, summary) = self.locate_edit_plan(id, plan_id.clone()).await?;
        let resources = summary.as_ref().map_or_else(
            Vec::new,
            crate::edit_plan::EditPlanApprovalSummary::coordination_resources,
        );
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
                root,
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
        let (actor, mutation, root) = if let Some(path) = workspace_edit_path(&edit) {
            self.entry_for_path(id, &path).await?
        } else {
            let (identity, actor, mutation) = self.entry(id).await?;
            (actor, mutation, identity.root().as_path().to_path_buf())
        };
        let _mutation = mutation.lock().await;
        actor
            .preview_edit(id.as_str().to_string(), edit, encoding, root)
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
        let file_path = match &request {
            GeneratedEditRequest::Rename { file_path, .. }
            | GeneratedEditRequest::Format { file_path, .. }
            | GeneratedEditRequest::RangeFormat { file_path, .. }
            | GeneratedEditRequest::MoveItem { file_path, .. } => file_path,
        };
        let (actor, mutation, root) = self.entry_for_path(id, Path::new(file_path)).await?;
        let _mutation = mutation.lock().await;
        actor
            .preview_generated_edit(id.as_str().to_string(), request, encoding, root)
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
        let (actor, mutation, root) = self.entry_for_path(id, Path::new(&file_path)).await?;
        let _mutation = mutation.lock().await;
        actor
            .move_inline_module_preview(
                id.as_str().to_string(),
                file_path,
                module_name,
                module_position,
                encoding,
                root,
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
        let (actor, mutation, root) = self
            .entry_for_path(id, Path::new(&request.file_path))
            .await?;
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
    ///
    /// # Errors
    ///
    /// Returns an error when the path is not registered or activation fails.
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

    /// Resolve a project ID and file path to the actor owning the longest
    /// matching registered root, waking the project when it is dormant.
    ///
    /// # Errors
    ///
    /// Returns an error when the project or path is not registered, or when
    /// the owning actor cannot be activated or reached.
    pub async fn active_actor_for_project_path(
        &self,
        id: &ProjectId,
        path: impl AsRef<Path>,
    ) -> Result<ProjectHandle, ProjectRegistryError> {
        let (actor, _, _) = self.entry_for_path(id, path.as_ref()).await?;
        if actor.query().await?.status() == ProjectStatus::Dormant {
            self.activate(id).await?;
        }
        actor.wait_until_routable().await?;
        Ok(actor)
    }

    /// Resolve a dependency source previously surfaced by an active LSP.
    ///
    /// # Errors
    ///
    /// Returns an error when the path cannot be canonicalized or no actor owns it.
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
        let candidates = self.path_candidates(path.as_ref()).await?;
        self.projects
            .read()
            .await
            .values()
            .flat_map(|project| {
                candidates.iter().filter_map(|canonical| {
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
            })
            .max_by_key(|(components, _, _)| *components)
            .map(|(_, project_id, actor)| (project_id, actor))
            .ok_or_else(|| ProjectIdentityError::UnregisteredPath(candidates[0].clone()).into())
    }

    /// Resolve a project-relative or absolute path to its canonical registered path.
    ///
    /// Relative paths are checked against every registered root so source paths
    /// returned by project-scoped tools can be passed directly to path-based tools.
    ///
    /// # Errors
    ///
    /// Returns an identity error when the path cannot be canonicalized or is not
    /// contained by a registered project root.
    pub async fn canonical_registered_path(
        &self,
        path: impl AsRef<Path>,
    ) -> Result<PathBuf, ProjectRegistryError> {
        let candidates = self.path_candidates(path.as_ref()).await?;
        let projects = self.projects.read().await;
        candidates
            .iter()
            .find(|canonical| {
                projects.values().any(|project| {
                    project
                        .identity
                        .roots()
                        .iter()
                        .any(|root| canonical.starts_with(root.as_path()))
                })
            })
            .cloned()
            .ok_or_else(|| ProjectIdentityError::UnregisteredPath(candidates[0].clone()).into())
    }

    async fn path_candidates(
        &self,
        requested: &Path,
    ) -> Result<Vec<PathBuf>, ProjectRegistryError> {
        let mut candidates = canonicalize(requested).ok().into_iter().collect::<Vec<_>>();
        if requested.is_relative() {
            let roots = self
                .projects
                .read()
                .await
                .values()
                .flat_map(|project| project.identity.roots().iter())
                .map(|root| root.as_path().to_owned())
                .collect::<Vec<_>>();
            candidates.extend(
                roots
                    .iter()
                    .filter_map(|root| canonicalize(&root.join(requested)).ok()),
            );
        }
        if candidates.is_empty() {
            let canonical = canonicalize(requested)?;
            return Err(ProjectIdentityError::UnregisteredPath(canonical).into());
        }
        Ok(candidates)
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
                    if error.contains("invalid_symbol_handle:") {
                        continue;
                    }
                    return Err(error);
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

    pub(crate) fn remove_deferred_resource(&self, token: &str) {
        self.deferred_results
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(token);
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

    pub(super) async fn actor_entries(
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

    pub(super) async fn entry(
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

    async fn entry_for_path(
        &self,
        id: &ProjectId,
        path: &Path,
    ) -> Result<(ProjectHandle, MutationGate, PathBuf), ProjectRegistryError> {
        let path = canonicalize_routing_path(path)?;
        let projects = self.projects.read().await;
        let result = (|| {
            let project = projects
                .get(id)
                .ok_or_else(|| ProjectRegistryError::ProjectNotFound(id.clone()))?;
            let roots = project
                .identity
                .roots()
                .iter()
                .map(CanonicalRoot::as_path)
                .map(Path::to_path_buf)
                .collect::<Vec<_>>();
            let root = longest_matching_root(&path, &roots).ok_or_else(|| {
                ProjectIdentityError::ProjectPathMismatch {
                    id: id.clone(),
                    path: path.clone(),
                }
            })?;
            let actor = project.actor_for_root(root).ok_or_else(|| {
                ProjectIdentityError::ProjectPathMismatch {
                    id: id.clone(),
                    path: path.clone(),
                }
            })?;
            Ok((
                actor.actor.clone(),
                actor.mutation.clone(),
                root.to_path_buf(),
            ))
        })();
        drop(projects);
        result
    }

    async fn locate_code_action(
        &self,
        id: &ProjectId,
        action_id: PlanId,
    ) -> Result<(ProjectHandle, MutationGate, PathBuf), ProjectRegistryError> {
        let candidates = self
            .projects
            .read()
            .await
            .get(id)
            .map(|project| {
                project
                    .actors
                    .iter()
                    .filter_map(|entry| {
                        entry.roots.first().map(|root| {
                            (
                                entry.actor.clone(),
                                entry.mutation.clone(),
                                root.as_path().to_path_buf(),
                            )
                        })
                    })
                    .collect::<Vec<_>>()
            })
            .ok_or_else(|| ProjectRegistryError::ProjectNotFound(id.clone()))?;
        for (actor, mutation, root) in candidates {
            if actor.has_code_action(action_id.clone()).await? {
                return Ok((actor, mutation, root));
            }
        }
        Err(ProjectRegistryError::Actor(ProjectActorError::Operation(
            format!("code action reference is missing or expired: {action_id}"),
        )))
    }
}

fn workspace_edit_path(edit: &WorkspaceEdit) -> Option<PathBuf> {
    normalize(edit.clone())
        .ok()?
        .operations
        .into_iter()
        .find_map(|operation| match operation {
            EditOperation::Text { uri, .. }
            | EditOperation::Create { uri, .. }
            | EditOperation::Delete { uri, .. } => uri_to_path(&uri),
            EditOperation::Rename { old_uri, .. } => uri_to_path(&old_uri),
        })
}

fn canonicalize_routing_path(path: &Path) -> Result<PathBuf, ProjectRegistryError> {
    let error = match canonicalize(path) {
        Ok(path) => return Ok(path),
        Err(error) => error,
    };
    let Some(file_name) = path.file_name() else {
        return Err(error.into());
    };
    let Some(parent) = path.parent() else {
        return Err(error.into());
    };
    Ok(canonicalize(parent)?.join(file_name))
}

pub(super) fn aggregate_statuses(
    statuses: impl IntoIterator<Item = ProjectStatus>,
) -> ProjectStatus {
    statuses
        .into_iter()
        .max_by_key(|status| project_status_priority(*status))
        .unwrap_or(ProjectStatus::Starting)
}

pub(super) fn unique_mutation_gates(
    projects: &HashMap<ProjectId, ProjectEntry>,
) -> Vec<MutationGate> {
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

pub(super) fn reject_new_actor_work<'a>(actors: impl IntoIterator<Item = &'a ProjectHandle>) {
    for actor in actors {
        actor.reject_new_work();
    }
}

pub(super) fn shutdown_actor_groups(
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

pub(super) fn repository_project<'a>(
    projects: &'a HashMap<ProjectId, ProjectEntry>,
    identity: &ProjectIdentity,
) -> Option<&'a ProjectEntry> {
    let repository = identity.repository_identity()?;
    projects
        .values()
        .find(|project| project.identity.repository_identity() == Some(repository))
}

pub(super) fn translator_templates_match(
    left: Option<&TranslatorTemplate>,
    right: Option<&TranslatorTemplate>,
) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(left), Some(right)) => left.same_configuration(right),
        _ => false,
    }
}

pub(super) async fn save_persisted_state(
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

pub(super) async fn load_persisted_state(
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

pub(super) fn is_invalid_utf8_error(error: &crate::error::Error) -> bool {
    let (crate::error::Error::Io(source) | crate::error::Error::FileIo { source, .. }) = error
    else {
        return false;
    };
    source.kind() == std::io::ErrorKind::InvalidData
}

#[cfg(test)]
mod tests {
    use super::{ProjectStatus, aggregate_statuses};

    #[test]
    fn aggregate_statuses_uses_failure_as_the_highest_priority() {
        assert_eq!(
            aggregate_statuses([ProjectStatus::Ready, ProjectStatus::Failed]),
            ProjectStatus::Failed
        );
    }
}
