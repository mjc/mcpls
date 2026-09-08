//! Project lifecycle state and event history.

#[allow(clippy::wildcard_imports)]
use super::*;
#[allow(clippy::wildcard_imports)]
use super::{identity::*, runtime::*};

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
    pub(super) reason: ProjectDormancyReason,
    pub(super) idle_for: Option<Duration>,
}

impl ProjectDormancy {
    pub(super) const fn new(reason: ProjectDormancyReason, idle_for: Option<Duration>) -> Self {
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
    pub(super) status: ProjectStatus,
    pub(super) last_error: Option<String>,
    pub(super) dormancy: Option<ProjectDormancy>,
    pub(super) runtime: ProjectRuntimeSummary,
}

impl ProjectState {
    pub(super) const fn new(status: ProjectStatus, runtime: ProjectRuntimeSummary) -> Self {
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

    pub(super) fn sync_runtime(&mut self, runtime: &ProjectRuntime) {
        self.runtime = runtime.summary();
    }

    pub(super) fn aggregate(states: impl IntoIterator<Item = Self>) -> Self {
        let mut states = states.into_iter();
        let Some(mut aggregate) = states.next() else {
            return Self::new(ProjectStatus::Starting, ProjectRuntimeSummary::default());
        };
        for state in states {
            aggregate.merge(state);
        }
        aggregate
    }

    pub(super) fn merge(&mut self, state: Self) {
        let state_priority = project_status_priority(state.status);
        if state_priority >= project_status_priority(self.status) {
            self.status = state.status;
            self.last_error = state.last_error;
            self.dormancy = state.dormancy;
        }
        self.runtime.merge(state.runtime);
    }
}

pub(super) const fn project_status_priority(status: ProjectStatus) -> u8 {
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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum SemanticReadiness {
    Loading,
    #[default]
    Ready,
}

#[cfg(test)]
mod tests {
    use super::{ProjectRuntimeSummary, ProjectState, ProjectStatus};

    #[test]
    fn aggregate_preserves_the_highest_priority_lifecycle_state() {
        let aggregate = ProjectState::aggregate([
            ProjectState::new(ProjectStatus::Ready, ProjectRuntimeSummary::default()),
            ProjectState::new(ProjectStatus::Failed, ProjectRuntimeSummary::default()),
        ]);

        assert_eq!(aggregate.status(), ProjectStatus::Failed);
    }
}

/// Project-local state counts and roots owned by an actor.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProjectRuntimeSummary {
    workspace_roots: Vec<PathBuf>,
    configured_language_ids: Vec<String>,
    active_language_ids: Vec<String>,
    pub(super) semantic_readiness: SemanticReadiness,
    open_document_count: usize,
    generation: u64,
}

impl ProjectRuntimeSummary {
    pub(super) fn from_translator(translator: &Translator, generation: u64) -> Self {
        Self {
            workspace_roots: translator.workspace_roots().to_vec(),
            configured_language_ids: translator.configured_language_ids(),
            active_language_ids: translator.active_language_ids(),
            semantic_readiness: if translator.is_initializing() {
                SemanticReadiness::Loading
            } else {
                SemanticReadiness::Ready
            },
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

    pub(super) fn merge(&mut self, other: Self) {
        self.workspace_roots.extend(other.workspace_roots);
        self.configured_language_ids
            .extend(other.configured_language_ids);
        self.active_language_ids.extend(other.active_language_ids);
        if other.semantic_readiness == SemanticReadiness::Loading {
            self.semantic_readiness = SemanticReadiness::Loading;
        }
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
