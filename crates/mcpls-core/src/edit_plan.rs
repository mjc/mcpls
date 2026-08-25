//! Preview artifacts and bounded storage for workspace edit plans.

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::fmt::Write as _;
use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use similar::{ChangeTag, TextDiff};
use uuid::Uuid;

use crate::edit_coordinator::EditResource;
use crate::edit_paths::FileOperation;
use crate::edit_policy::{EditMode, EditPolicy};

/// Shared edit safety limits used by preview and project-local plan storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EditLimits {
    /// Maximum number of affected files retained in one project store.
    pub max_files: usize,
    /// Maximum number of text edits accepted by preview planning.
    pub max_edits: usize,
    /// Maximum combined plan bytes retained by one project store.
    pub max_bytes: usize,
    /// Maximum combined original and planned bytes for one file.
    pub max_file_bytes: usize,
    /// Maximum create, rename, and delete operations in one preview.
    pub max_resource_operations: usize,
    /// Lifetime of a stored plan.
    pub plan_ttl: Duration,
}

impl EditLimits {
    /// Default limits for one long-lived project actor.
    pub const PROJECT: Self = Self {
        max_files: 64,
        max_edits: 4_096,
        max_bytes: 16 * 1024 * 1024,
        max_file_bytes: 8 * 1024 * 1024,
        max_resource_operations: 256,
        plan_ttl: Duration::from_secs(15 * 60),
    };
}

/// Opaque identifier for one preview/apply transaction.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PlanId(String);

impl PlanId {
    /// Generate a fresh, unguessable plan identifier.
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4().to_string())
    }

    /// Return the serialized identifier.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Parse an opaque identifier received from an external caller.
    ///
    /// The identifier remains opaque to callers; parsing only rejects an
    /// empty value so a missing plan cannot be confused with a valid token.
    ///
    /// # Errors
    ///
    /// Returns [`PlanIdError::Empty`] when the supplied value is blank.
    pub fn parse(value: impl Into<String>) -> Result<Self, PlanIdError> {
        let value = value.into();
        if value.trim().is_empty() {
            Err(PlanIdError::Empty)
        } else {
            Ok(Self(value))
        }
    }
}

/// Invalid externally supplied plan identifier.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PlanIdError {
    /// The identifier was empty or only whitespace.
    #[error("edit plan ID must not be empty")]
    Empty,
}

impl Default for PlanId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for PlanId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Source of the content captured by a plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotSource {
    /// Content read from the filesystem.
    Disk,
    /// Content held by the project's open-document tracker.
    OpenDocument,
}

/// Exact pre-edit content and metadata for one affected file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSnapshot {
    path: PathBuf,
    source: SnapshotSource,
    version: Option<i32>,
    created: bool,
    content_hash: String,
    original_content: String,
    planned_content: String,
}

/// Failure while validating a preview snapshot before application.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SnapshotValidationError {
    /// The current content no longer matches the preview hash.
    #[error("file content changed since preview: {path}")]
    ContentChanged {
        /// Affected file path.
        path: PathBuf,
    },
    /// An open document version no longer matches the preview.
    #[error("document version changed for {path}: expected {expected}, got {actual:?}")]
    VersionChanged {
        /// Affected file path.
        path: PathBuf,
        /// Version captured by the preview.
        expected: i32,
        /// Current document version.
        actual: Option<i32>,
    },
}

impl FileSnapshot {
    /// Capture content and compute its SHA-256 precondition hash.
    #[must_use]
    pub fn from_contents(
        path: PathBuf,
        source: SnapshotSource,
        version: Option<i32>,
        original_content: impl Into<String>,
        planned_content: impl Into<String>,
    ) -> Self {
        let original_content = original_content.into();
        let planned_content = planned_content.into();
        let content_hash = hash_content(&original_content);
        Self {
            path,
            source,
            version,
            created: false,
            content_hash,
            original_content,
            planned_content,
        }
    }

    /// Capture the empty pre-image of a file that must be created by the plan.
    #[must_use]
    pub fn from_created_contents(path: PathBuf, planned_content: impl Into<String>) -> Self {
        let mut snapshot =
            Self::from_contents(path, SnapshotSource::Disk, None, "", planned_content);
        snapshot.created = true;
        snapshot
    }

    /// Return the canonical path captured by this snapshot.
    #[must_use]
    pub const fn path(&self) -> &PathBuf {
        &self.path
    }

    /// Return whether the source was disk or an open document.
    #[must_use]
    pub const fn source(&self) -> SnapshotSource {
        self.source
    }

    /// Return the expected open-document version, when supplied.
    #[must_use]
    pub const fn version(&self) -> Option<i32> {
        self.version
    }

    /// Return whether the path was absent when this snapshot was captured.
    #[must_use]
    pub const fn was_created(&self) -> bool {
        self.created
    }

    /// Return the SHA-256 hash of the exact pre-edit content.
    #[must_use]
    pub fn content_hash(&self) -> &str {
        &self.content_hash
    }

    /// Return the exact pre-edit content.
    #[must_use]
    pub fn original_content(&self) -> &str {
        &self.original_content
    }

    /// Return the deterministic planned content.
    #[must_use]
    pub fn planned_content(&self) -> &str {
        &self.planned_content
    }

    /// Validate exact content and an optional open-document version.
    ///
    /// # Errors
    ///
    /// Returns a fail-closed error if either the content hash or captured
    /// document version differs from the current state.
    pub fn validate(
        &self,
        current_content: &str,
        current_version: Option<i32>,
    ) -> Result<(), SnapshotValidationError> {
        if hash_content(current_content) != self.content_hash {
            return Err(SnapshotValidationError::ContentChanged {
                path: self.path.clone(),
            });
        }
        if let Some(expected) = self.version
            && current_version != Some(expected)
        {
            return Err(SnapshotValidationError::VersionChanged {
                path: self.path.clone(),
                expected,
                actual: current_version,
            });
        }
        Ok(())
    }
}

const MAX_RENDERED_DIFF_BYTES: usize = 64 * 1024;
const MAX_DIFF_COMPUTE_TIME: Duration = Duration::from_millis(500);
const DIFF_TRUNCATION_MARKER: &str = "\n... diff truncated ...\n";

/// Complete line-change counts for one previewed file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDiffSummary {
    path: PathBuf,
    additions: usize,
    deletions: usize,
}

impl FileDiffSummary {
    /// Return the changed file path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Return the number of inserted lines.
    #[must_use]
    pub const fn additions(&self) -> usize {
        self.additions
    }

    /// Return the number of deleted lines.
    #[must_use]
    pub const fn deletions(&self) -> usize {
        self.deletions
    }
}

/// Immutable preview artifact bound to one project identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditPlan {
    id: PlanId,
    project_id: String,
    workspace_root: Option<PathBuf>,
    files: Vec<FileSnapshot>,
    operations: Vec<String>,
    file_operations: Vec<FileOperation>,
    unified_diff: String,
    diff_files: Vec<FileDiffSummary>,
    diff_truncated: bool,
    safe_to_apply: bool,
    policy_generation: u64,
    created_at: SystemTime,
    expires_at: SystemTime,
    estimated_bytes: usize,
}

/// Bounded metadata used to ask for approval without exposing plan contents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EditPlanApprovalSummary {
    pub(crate) plan_id: PlanId,
    pub(crate) project_id: String,
    pub(crate) affected_files: Vec<PathBuf>,
    pub(crate) operations: Vec<String>,
    pub(crate) file_operations: Vec<FileOperation>,
    pub(crate) diff_files: Vec<FileDiffSummary>,
    pub(crate) diff_truncated: bool,
    pub(crate) safe_to_apply: bool,
    pub(crate) snapshot_hashes: Vec<String>,
    pub(crate) versions: Vec<Option<i32>>,
}

impl EditPlanApprovalSummary {
    /// Return the complete path set that an apply may mutate.
    pub(crate) fn coordination_resources(&self) -> Vec<EditResource> {
        let mut resources = self
            .affected_files
            .iter()
            .cloned()
            .map(EditResource::exact)
            .collect::<Vec<_>>();
        for operation in &self.file_operations {
            match operation {
                FileOperation::Create { path, .. } => {
                    resources.push(EditResource::exact(path.clone()))
                }
                FileOperation::Rename { from, to, .. } => {
                    let directory = from.is_dir();
                    resources.push(if directory {
                        EditResource::directory(from.clone())
                    } else {
                        EditResource::exact(from.clone())
                    });
                    resources.push(if directory {
                        EditResource::directory(to.clone())
                    } else {
                        EditResource::exact(to.clone())
                    });
                }
                FileOperation::Delete { path, recursive } => resources.push(if *recursive {
                    EditResource::directory(path.clone())
                } else {
                    EditResource::exact(path.clone())
                }),
            }
        }
        resources
    }
}

impl EditPlan {
    /// Build a plan from exact snapshots and preview metadata.
    #[must_use]
    pub fn new(
        project_id: String,
        files: Vec<FileSnapshot>,
        operations: Vec<String>,
        safe_to_apply: bool,
        ttl: Duration,
    ) -> Self {
        let (unified_diff, diff_files, diff_truncated) = render_unified_diff(&files);
        let created_at = SystemTime::now();
        let expires_at = created_at
            .checked_add(ttl)
            .unwrap_or(SystemTime::UNIX_EPOCH);
        let estimated_bytes = project_id.len()
            + unified_diff.len()
            + operations.iter().map(String::len).sum::<usize>()
            + files
                .iter()
                .map(|file| file.original_content.len() + file.planned_content.len())
                .sum::<usize>();
        Self {
            id: PlanId::new(),
            project_id,
            workspace_root: None,
            files,
            operations,
            file_operations: Vec::new(),
            unified_diff,
            diff_files,
            diff_truncated,
            safe_to_apply,
            policy_generation: 0,
            created_at,
            expires_at,
            estimated_bytes,
        }
    }

    /// Return the opaque plan ID.
    #[must_use]
    pub const fn id(&self) -> &PlanId {
        &self.id
    }

    /// Return the owning project ID.
    #[must_use]
    pub fn project_id(&self) -> &str {
        &self.project_id
    }

    /// Bind this plan to the canonical workspace root used during preview.
    #[must_use]
    pub fn with_workspace_root(mut self, root: PathBuf) -> Self {
        self.workspace_root = Some(root);
        self
    }

    /// Return the canonical workspace root captured during preview, when known.
    #[must_use]
    pub fn workspace_root(&self) -> Option<&Path> {
        self.workspace_root.as_deref()
    }

    /// Return affected file snapshots.
    #[must_use]
    pub fn files(&self) -> &[FileSnapshot] {
        &self.files
    }

    /// Return snapshots whose source is actor-owned open-document state.
    #[must_use = "iterate the open-document snapshots"]
    pub fn open_document_snapshots(&self) -> impl Iterator<Item = &FileSnapshot> {
        self.files
            .iter()
            .filter(|snapshot| snapshot.source() == SnapshotSource::OpenDocument)
    }

    /// Return planned file operations and other preview descriptors.
    #[must_use]
    pub fn operations(&self) -> &[String] {
        &self.operations
    }

    /// Attach validated resource operations to this plan.
    #[must_use]
    pub fn with_file_operations(mut self, file_operations: Vec<FileOperation>) -> Self {
        self.estimated_bytes = self.estimated_bytes.saturating_add(
            file_operations
                .iter()
                .map(file_operation_bytes)
                .sum::<usize>(),
        );
        self.file_operations = file_operations;
        self
    }

    /// Return resource operations retained for apply-time revalidation.
    #[must_use]
    pub fn file_operations(&self) -> &[FileOperation] {
        &self.file_operations
    }

    /// Return the unified diff for changed text files.
    #[must_use]
    pub fn unified_diff(&self) -> &str {
        &self.unified_diff
    }

    /// Return complete per-file line counts, even when rendered diff text was truncated.
    #[must_use]
    pub fn diff_files(&self) -> &[FileDiffSummary] {
        &self.diff_files
    }

    /// Return whether the rendered diff text reached its response-size bound.
    #[must_use]
    pub const fn diff_truncated(&self) -> bool {
        self.diff_truncated
    }

    /// Return whether all preconditions currently allow application.
    #[must_use]
    pub const fn safe_to_apply(&self) -> bool {
        self.safe_to_apply
    }

    /// Return the policy generation that admitted this plan to a store.
    #[must_use]
    pub const fn policy_generation(&self) -> u64 {
        self.policy_generation
    }

    /// Return the plan creation timestamp.
    #[must_use]
    pub const fn created_at(&self) -> SystemTime {
        self.created_at
    }

    /// Return whether the plan has expired at the supplied time.
    #[must_use]
    pub fn is_expired(&self, now: SystemTime) -> bool {
        now >= self.expires_at
    }

    /// Return the approximate bounded-store byte cost.
    #[must_use]
    pub const fn estimated_bytes(&self) -> usize {
        self.estimated_bytes
    }

    /// Return approval metadata without copying original or planned source.
    #[must_use]
    pub(crate) fn approval_summary(&self) -> EditPlanApprovalSummary {
        EditPlanApprovalSummary {
            plan_id: self.id.clone(),
            project_id: self.project_id.clone(),
            affected_files: self.files.iter().map(|file| file.path.clone()).collect(),
            operations: self.operations.clone(),
            file_operations: self.file_operations.clone(),
            diff_files: self.diff_files.clone(),
            diff_truncated: self.diff_truncated,
            safe_to_apply: self.safe_to_apply,
            snapshot_hashes: self
                .files
                .iter()
                .map(|file| file.content_hash.clone())
                .collect(),
            versions: self.files.iter().map(|file| file.version).collect(),
        }
    }
}

/// Outcome recorded when a stored edit plan is applied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EditAuditOutcome {
    /// Every planned operation committed successfully.
    Committed,
    /// Application failed before a complete commit.
    Failed {
        /// Redacted, human-readable failure summary.
        error: String,
    },
}

/// Bounded audit record for one plan application attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditAuditRecord {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    principal: Option<String>,
    timestamp_ms: u64,
    project_id: String,
    plan_id: String,
    operations: Vec<String>,
    precondition_hashes: Vec<String>,
    versions: Vec<Option<i32>>,
    committed_files: Vec<PathBuf>,
    outcome: EditAuditOutcome,
    rollback: bool,
}

impl EditAuditRecord {
    /// Create an audit record containing plan metadata without file contents.
    #[must_use]
    pub fn for_plan(plan: &EditPlan) -> Self {
        Self::for_plan_with_context(plan, None, None)
    }

    /// Create an audit record with optional caller context and no file contents.
    #[must_use]
    pub fn for_plan_with_context(
        plan: &EditPlan,
        session_id: Option<String>,
        principal: Option<String>,
    ) -> Self {
        Self {
            session_id,
            principal,
            timestamp_ms: SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
                .try_into()
                .unwrap_or(u64::MAX),
            project_id: plan.project_id().to_owned(),
            plan_id: plan.id().as_str().to_owned(),
            operations: plan.operations().to_vec(),
            precondition_hashes: plan
                .files()
                .iter()
                .map(|file| file.content_hash().to_owned())
                .collect(),
            versions: plan.files().iter().map(FileSnapshot::version).collect(),
            committed_files: Vec::new(),
            outcome: EditAuditOutcome::Failed {
                error: "application pending".to_owned(),
            },
            rollback: false,
        }
    }

    /// Return the caller session when the transport supplied one.
    #[must_use]
    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    /// Return the authenticated or configured principal when available.
    #[must_use]
    pub fn principal(&self) -> Option<&str> {
        self.principal.as_deref()
    }

    /// Return a completed success record with committed file paths.
    #[must_use]
    pub fn committed(mut self, files: Vec<PathBuf>) -> Self {
        self.committed_files = files;
        self.outcome = EditAuditOutcome::Committed;
        self
    }

    /// Return a completed failure record.
    #[must_use]
    pub fn failed(mut self, error: impl Into<String>, rollback: bool) -> Self {
        self.outcome = EditAuditOutcome::Failed {
            error: error.into(),
        };
        self.rollback = rollback;
        self
    }

    /// Return the project that owned the plan.
    #[must_use]
    pub fn project_id(&self) -> &str {
        &self.project_id
    }

    /// Return the plan identifier.
    #[must_use]
    pub fn plan_id(&self) -> &str {
        &self.plan_id
    }

    /// Return the recorded operation descriptions.
    #[must_use]
    pub fn operations(&self) -> &[String] {
        &self.operations
    }

    /// Return precondition hashes without exposing source contents.
    #[must_use]
    pub fn precondition_hashes(&self) -> &[String] {
        &self.precondition_hashes
    }

    /// Return the optimistic document versions captured by the plan.
    #[must_use]
    pub fn versions(&self) -> &[Option<i32>] {
        &self.versions
    }

    /// Return the files committed by a successful application.
    #[must_use]
    pub fn committed_files(&self) -> &[PathBuf] {
        &self.committed_files
    }

    /// Return the application outcome.
    #[must_use]
    pub const fn outcome(&self) -> &EditAuditOutcome {
        &self.outcome
    }

    /// Return whether the failed application rolled back staged files.
    #[must_use]
    pub const fn rollback(&self) -> bool {
        self.rollback
    }
}

/// Whether an audit sink failure blocks an otherwise successful edit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditFailureMode {
    /// Keep the bounded in-memory record and allow the edit to continue.
    #[default]
    FailOpen,
    /// Return an error after a successful edit when the durable record fails.
    FailClosed,
}

/// Configuration for a bounded JSONL audit sink.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditLogPolicy {
    path: PathBuf,
    max_bytes: usize,
    failure_mode: AuditFailureMode,
}

impl AuditLogPolicy {
    /// Construct a bounded audit sink policy.
    ///
    /// # Errors
    ///
    /// Returns [`AuditLogError::InvalidMaxBytes`] when the sink limit is zero.
    pub fn new(
        path: impl Into<PathBuf>,
        max_bytes: usize,
        failure_mode: AuditFailureMode,
    ) -> Result<Self, AuditLogError> {
        if max_bytes == 0 {
            return Err(AuditLogError::InvalidMaxBytes);
        }
        Ok(Self {
            path: path.into(),
            max_bytes,
            failure_mode,
        })
    }

    /// Return the JSONL destination path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Return the configured sink failure behavior.
    #[must_use]
    pub const fn failure_mode(&self) -> AuditFailureMode {
        self.failure_mode
    }
}

/// Invalid audit sink configuration.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum AuditLogError {
    /// The sink must have room for at least one byte.
    #[error("audit log max bytes must be greater than zero")]
    InvalidMaxBytes,
}

/// Errors returned by bounded plan storage and project-scoped lookup.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PlanStoreError {
    /// The plan exceeds the store's byte limit.
    #[error("edit plan exceeds the {limit} byte store limit: {actual} bytes")]
    TooLarge {
        /// Configured byte limit.
        limit: usize,
        /// Plan's estimated size.
        actual: usize,
    },
    /// A plan ID was not found or has expired.
    #[error("edit plan not found: {0}")]
    NotFound(PlanId),
    /// The plan expired before it was consumed.
    #[error("edit plan expired: {0}")]
    Expired(PlanId),
    /// The plan was evicted by a store quota before it was consumed.
    #[error("edit plan evicted by store quota: {0}")]
    Evicted(PlanId),
    /// The plan belongs to a different project.
    #[error("edit plan belongs to project {actual}, not {expected}")]
    ProjectMismatch {
        /// Project requested by the caller.
        expected: String,
        /// Project recorded on the plan.
        actual: String,
    },
    /// The plan was created before the current edit policy generation.
    #[error("edit plan invalidated by a policy change: {plan_id}")]
    PolicyChanged {
        /// Invalidated plan identifier.
        plan_id: PlanId,
        /// Generation captured by the plan.
        plan_generation: u64,
        /// Current store generation.
        current_generation: u64,
    },
    /// A configured durable audit sink could not accept a record.
    #[error("failed to write audit log {path}: {error}")]
    Audit {
        /// Configured audit destination.
        path: PathBuf,
        /// Redacted write failure summary.
        error: String,
    },
}

/// Bounded, project-local in-memory plan storage.
#[derive(Debug)]
pub struct EditPlanStore {
    plans: HashMap<PlanId, EditPlan>,
    audit_records: VecDeque<EditAuditRecord>,
    max_audit_records: usize,
    max_plans: usize,
    max_bytes: usize,
    ttl: Duration,
    bytes: usize,
    policy_generation: u64,
    policy: EditPolicy,
    tombstones: PlanTombstones,
    audit_log: Option<AuditLogPolicy>,
}

impl EditPlanStore {
    /// Construct the daemon's bounded project-local plan store.
    #[must_use]
    pub fn for_project() -> Self {
        let limits = EditLimits::PROJECT;
        Self::new(limits.max_files, limits.max_bytes, limits.plan_ttl)
    }

    /// Create a bounded plan store.
    #[must_use]
    pub fn new(max_plans: usize, max_bytes: usize, ttl: Duration) -> Self {
        Self {
            plans: HashMap::new(),
            audit_records: VecDeque::new(),
            max_audit_records: max_plans.max(1).saturating_mul(8).max(16),
            max_plans: max_plans.max(1),
            max_bytes: max_bytes.max(1),
            ttl,
            bytes: 0,
            policy_generation: 0,
            policy: EditPolicy::new(EditMode::Write),
            tombstones: PlanTombstones::new(max_plans),
            audit_log: None,
        }
    }

    /// Insert a plan, evicting expired and oldest entries as needed.
    ///
    /// # Errors
    ///
    /// Returns [`PlanStoreError::TooLarge`] when the plan cannot fit even in
    /// an empty store.
    pub fn insert(&mut self, mut plan: EditPlan) -> Result<(), PlanStoreError> {
        if plan.estimated_bytes() > self.max_bytes {
            return Err(PlanStoreError::TooLarge {
                limit: self.max_bytes,
                actual: plan.estimated_bytes(),
            });
        }
        let now = SystemTime::now();
        self.purge_expired(now);
        plan.expires_at = now.checked_add(self.ttl).unwrap_or(now);
        plan.policy_generation = self.policy_generation;
        if let Some(previous) = self.plans.remove(plan.id()) {
            self.bytes = self.bytes.saturating_sub(previous.estimated_bytes());
        }
        while self.plans.len() >= self.max_plans
            || self.bytes.saturating_add(plan.estimated_bytes()) > self.max_bytes
        {
            let Some(oldest_id) = self
                .plans
                .values()
                .min_by_key(|entry| entry.created_at())
                .map(|entry| entry.id().clone())
            else {
                break;
            };
            if let Some(oldest) = self.plans.remove(&oldest_id) {
                self.bytes = self.bytes.saturating_sub(oldest.estimated_bytes());
                self.tombstones.remember_evicted(oldest_id);
            }
        }
        self.bytes = self.bytes.saturating_add(plan.estimated_bytes());
        self.plans.insert(plan.id().clone(), plan);
        Ok(())
    }

    /// Look up a non-expired plan by ID.
    #[must_use]
    pub fn get(&self, id: &PlanId) -> Option<&EditPlan> {
        self.current_plan(id).ok()
    }

    /// Look up a plan while enforcing its owning project identity.
    ///
    /// # Errors
    ///
    /// Returns [`PlanStoreError::NotFound`] for an expired or unknown ID and
    /// [`PlanStoreError::ProjectMismatch`] when the caller selects another
    /// project.
    pub fn get_for_project(
        &self,
        id: &PlanId,
        project_id: &str,
    ) -> Result<&EditPlan, PlanStoreError> {
        let plan = self.current_plan(id)?;
        if plan.project_id() != project_id {
            return Err(PlanStoreError::ProjectMismatch {
                expected: project_id.to_string(),
                actual: plan.project_id().to_string(),
            });
        }
        Ok(plan)
    }

    /// Remove and return a plan for one project, consuming its apply token.
    ///
    /// Removing before the filesystem effect makes a plan single-use even
    /// when the caller retries after a partial commit or process failure.
    ///
    /// # Errors
    ///
    /// Returns [`PlanStoreError::NotFound`] for an expired or unknown plan and
    /// [`PlanStoreError::ProjectMismatch`] when the project does not own it.
    pub fn take_for_project(
        &mut self,
        id: &PlanId,
        project_id: &str,
    ) -> Result<EditPlan, PlanStoreError> {
        let plan = self.get_for_project(id, project_id)?.clone();
        self.bytes = self.bytes.saturating_sub(plan.estimated_bytes());
        self.plans.remove(id);
        Ok(plan)
    }

    /// Remove expired plans using an explicit clock value.
    pub fn purge_expired(&mut self, now: SystemTime) {
        let expired: Vec<_> = self
            .plans
            .values()
            .filter(|plan| plan.is_expired(now))
            .map(|plan| plan.id().clone())
            .collect();
        for id in expired {
            if let Some(plan) = self.plans.remove(&id) {
                self.bytes = self.bytes.saturating_sub(plan.estimated_bytes());
                self.tombstones.remember_expired(id);
            }
        }
    }

    /// Return the number of stored plans.
    #[must_use]
    pub fn len(&self) -> usize {
        self.plans.len()
    }

    /// Return whether no plans are stored.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.plans.is_empty()
    }

    /// Return the currently accounted approximate bytes.
    #[must_use]
    pub const fn bytes(&self) -> usize {
        self.bytes
    }

    /// Append one bounded audit record, evicting the oldest record if needed.
    pub fn record_audit(&mut self, record: EditAuditRecord) {
        if self.audit_records.len() >= self.max_audit_records {
            self.audit_records.pop_front();
        }
        self.audit_records.push_back(record);
    }

    /// Configure a durable JSONL sink for future audit records.
    pub fn set_audit_log(&mut self, policy: AuditLogPolicy) {
        self.audit_log = Some(policy);
    }

    /// Append an audit record to the configured sink and bounded memory store.
    ///
    /// With [`AuditFailureMode::FailOpen`], sink errors are ignored after the
    /// in-memory record is retained. With [`AuditFailureMode::FailClosed`], a
    /// sink error is returned and the record is not retained in memory.
    ///
    /// # Errors
    ///
    /// Returns [`PlanStoreError::Audit`] when a fail-closed sink rejects the
    /// serialized record.
    pub fn record_audit_with_policy(
        &mut self,
        record: EditAuditRecord,
    ) -> Result<(), PlanStoreError> {
        let Some(policy) = self.audit_log.clone() else {
            self.record_audit(record);
            return Ok(());
        };
        match append_audit_record(&policy, &record) {
            Ok(()) => {
                self.record_audit(record);
                Ok(())
            }
            Err(_error) if policy.failure_mode() == AuditFailureMode::FailOpen => {
                self.record_audit(record);
                Ok(())
            }
            Err(error) => Err(PlanStoreError::Audit {
                path: policy.path().to_path_buf(),
                error,
            }),
        }
    }

    /// Return audit records from oldest to newest.
    pub fn audit_records(&self) -> impl Iterator<Item = &EditAuditRecord> {
        self.audit_records.iter()
    }

    /// Replace the edit policy, invalidating plans if it changed.
    pub fn update_policy(&mut self, policy: EditPolicy) {
        if self.policy != policy {
            self.policy_generation = self.policy_generation.wrapping_add(1);
            self.policy = policy;
        }
    }

    fn current_plan(&self, id: &PlanId) -> Result<&EditPlan, PlanStoreError> {
        let Some(plan) = self.plans.get(id) else {
            return Err(self
                .tombstones
                .error_for(id)
                .unwrap_or_else(|| PlanStoreError::NotFound(id.clone())));
        };
        if plan.is_expired(SystemTime::now()) {
            return Err(PlanStoreError::Expired(id.clone()));
        }
        if plan.policy_generation() != self.policy_generation {
            return Err(PlanStoreError::PolicyChanged {
                plan_id: id.clone(),
                plan_generation: plan.policy_generation(),
                current_generation: self.policy_generation,
            });
        }
        Ok(plan)
    }
}

fn append_audit_record(policy: &AuditLogPolicy, record: &EditAuditRecord) -> Result<(), String> {
    let mut line = serde_json::to_vec(record).map_err(|error| error.to_string())?;
    line.push(b'\n');
    let existing = std::fs::metadata(policy.path()).map_or(0, |metadata| metadata.len());
    let total = existing
        .checked_add(u64::try_from(line.len()).map_err(|_| "audit record is too large".to_owned())?)
        .ok_or_else(|| "audit log size overflow".to_owned())?;
    if total > u64::try_from(policy.max_bytes).unwrap_or(u64::MAX) {
        return Err(format!("audit log exceeds {} byte limit", policy.max_bytes));
    }
    if let Some(parent) = policy.path().parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(policy.path())
        .map_err(|error| error.to_string())?;
    file.write_all(&line).map_err(|error| error.to_string())?;
    file.sync_all().map_err(|error| error.to_string())
}

#[derive(Debug)]
struct PlanTombstones {
    expired: VecDeque<PlanId>,
    evicted: VecDeque<PlanId>,
    limit: usize,
}

impl PlanTombstones {
    fn new(max_plans: usize) -> Self {
        Self {
            expired: VecDeque::new(),
            evicted: VecDeque::new(),
            limit: max_plans.max(1).saturating_mul(4).max(16),
        }
    }

    fn remember_expired(&mut self, id: PlanId) {
        remember_plan_id(&mut self.expired, self.limit, id);
    }

    fn remember_evicted(&mut self, id: PlanId) {
        remember_plan_id(&mut self.evicted, self.limit, id);
    }

    fn error_for(&self, id: &PlanId) -> Option<PlanStoreError> {
        if self.expired.contains(id) {
            Some(PlanStoreError::Expired(id.clone()))
        } else if self.evicted.contains(id) {
            Some(PlanStoreError::Evicted(id.clone()))
        } else {
            None
        }
    }
}

fn remember_plan_id(queue: &mut VecDeque<PlanId>, limit: usize, id: PlanId) {
    if queue.contains(&id) {
        return;
    }
    if queue.len() >= limit {
        queue.pop_front();
    }
    queue.push_back(id);
}

fn file_operation_bytes(operation: &FileOperation) -> usize {
    match operation {
        FileOperation::Create { path, .. } | FileOperation::Delete { path, .. } => {
            path.to_string_lossy().len()
        }
        FileOperation::Rename { from, to, .. } => from
            .to_string_lossy()
            .len()
            .saturating_add(to.to_string_lossy().len()),
    }
}

fn hash_content(content: &str) -> String {
    let mut hash = String::with_capacity(64);
    for byte in Sha256::digest(content.as_bytes()) {
        let _ = write!(&mut hash, "{byte:02x}");
    }
    hash
}

fn render_unified_diff(files: &[FileSnapshot]) -> (String, Vec<FileDiffSummary>, bool) {
    let mut rendered = String::new();
    let mut summaries = Vec::new();
    let mut truncated = false;
    let deadline = Instant::now() + MAX_DIFF_COMPUTE_TIME;

    for snapshot in files {
        if snapshot.original_content == snapshot.planned_content {
            continue;
        }
        let mut config = TextDiff::configure();
        config.deadline(deadline);
        let diff = config.diff_lines(&snapshot.original_content, &snapshot.planned_content);
        let mut additions = 0;
        let mut deletions = 0;
        for change in diff.iter_all_changes() {
            match change.tag() {
                ChangeTag::Insert => additions += 1,
                ChangeTag::Delete => deletions += 1,
                ChangeTag::Equal => {}
            }
        }
        summaries.push(FileDiffSummary {
            path: snapshot.path.clone(),
            additions,
            deletions,
        });

        if truncated {
            continue;
        }
        let path = snapshot.path.to_string_lossy();
        let file_diff = diff
            .unified_diff()
            .context_radius(3)
            .header(&path, &path)
            .to_string();
        truncated = append_bounded_diff(&mut rendered, &file_diff);
    }

    (rendered, summaries, truncated)
}

fn append_bounded_diff(rendered: &mut String, file_diff: &str) -> bool {
    let separator_len = usize::from(!rendered.is_empty());
    if rendered
        .len()
        .saturating_add(separator_len)
        .saturating_add(file_diff.len())
        <= MAX_RENDERED_DIFF_BYTES
    {
        if separator_len != 0 {
            rendered.push('\n');
        }
        rendered.push_str(file_diff);
        return false;
    }

    if separator_len != 0 {
        rendered.push('\n');
    }
    let text_limit = MAX_RENDERED_DIFF_BYTES - DIFF_TRUNCATION_MARKER.len();
    let available = text_limit.saturating_sub(rendered.len());
    let mut end = available.min(file_diff.len());
    while !file_diff.is_char_boundary(end) {
        end -= 1;
    }
    rendered.push_str(&file_diff[..end]);

    if rendered.len() > text_limit {
        let mut end = text_limit;
        while !rendered.is_char_boundary(end) {
            end -= 1;
        }
        rendered.truncate(end);
    }
    rendered.push_str(DIFF_TRUNCATION_MARKER);
    true
}

#[cfg(test)]
#[path = "edit_plan_tests.rs"]
mod tests;
