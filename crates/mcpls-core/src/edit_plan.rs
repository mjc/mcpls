//! Preview artifacts and bounded storage for workspace edit plans.

use std::collections::HashMap;
use std::fmt;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::edit_paths::FileOperation;

/// Shared edit safety limits used by preview and project-local plan storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EditLimits {
    /// Maximum number of affected files retained in one project store.
    pub max_files: usize,
    /// Maximum number of text edits accepted by preview planning.
    pub max_edits: usize,
    /// Maximum combined plan bytes retained by one project store.
    pub max_bytes: usize,
    /// Lifetime of a stored plan.
    pub plan_ttl: Duration,
}

impl EditLimits {
    /// Default limits for one long-lived project actor.
    pub const PROJECT: Self = Self {
        max_files: 64,
        max_edits: 4_096,
        max_bytes: 16 * 1024 * 1024,
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
            content_hash,
            original_content,
            planned_content,
        }
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

/// Immutable preview artifact bound to one project identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditPlan {
    id: PlanId,
    project_id: String,
    files: Vec<FileSnapshot>,
    operations: Vec<String>,
    file_operations: Vec<FileOperation>,
    unified_diff: String,
    safe_to_apply: bool,
    created_at: SystemTime,
    expires_at: SystemTime,
    estimated_bytes: usize,
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
        let unified_diff = files
            .iter()
            .map(unified_diff)
            .filter(|diff| !diff.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
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
            files,
            operations,
            file_operations: Vec::new(),
            unified_diff,
            safe_to_apply,
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

    /// Return whether all preconditions currently allow application.
    #[must_use]
    pub const fn safe_to_apply(&self) -> bool {
        self.safe_to_apply
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
    /// The plan belongs to a different project.
    #[error("edit plan belongs to project {actual}, not {expected}")]
    ProjectMismatch {
        /// Project requested by the caller.
        expected: String,
        /// Project recorded on the plan.
        actual: String,
    },
}

/// Bounded, project-local in-memory plan storage.
#[derive(Debug)]
pub struct EditPlanStore {
    plans: HashMap<PlanId, EditPlan>,
    max_plans: usize,
    max_bytes: usize,
    ttl: Duration,
    bytes: usize,
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
            max_plans: max_plans.max(1),
            max_bytes: max_bytes.max(1),
            ttl,
            bytes: 0,
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
            }
        }
        self.bytes = self.bytes.saturating_add(plan.estimated_bytes());
        self.plans.insert(plan.id().clone(), plan);
        Ok(())
    }

    /// Look up a non-expired plan by ID.
    #[must_use]
    pub fn get(&self, id: &PlanId) -> Option<&EditPlan> {
        self.plans
            .get(id)
            .filter(|plan| !plan.is_expired(SystemTime::now()))
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
        let plan = self
            .get(id)
            .ok_or_else(|| PlanStoreError::NotFound(id.clone()))?;
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

fn unified_diff(snapshot: &FileSnapshot) -> String {
    if snapshot.original_content == snapshot.planned_content {
        return String::new();
    }
    let mut diff = format!(
        "--- {}\n+++ {}\n@@\n",
        snapshot.path.display(),
        snapshot.path.display()
    );
    for line in snapshot.original_content.lines() {
        diff.push('-');
        diff.push_str(line);
        diff.push('\n');
    }
    for line in snapshot.planned_content.lines() {
        diff.push('+');
        diff.push_str(line);
        diff.push('\n');
    }
    diff
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use super::*;

    #[test]
    fn captures_snapshot_hash_diff_and_project_identity() {
        let snapshot = FileSnapshot::from_contents(
            PathBuf::from("src/lib.rs"),
            SnapshotSource::OpenDocument,
            Some(7),
            "fn old() {}\n",
            "fn new() {}\n",
        );
        let plan = EditPlan::new(
            "project-a".to_string(),
            vec![snapshot],
            vec!["text edit".to_string()],
            true,
            Duration::from_secs(60),
        );

        assert_eq!(plan.project_id(), "project-a");
        assert_eq!(plan.files()[0].version(), Some(7));
        assert_ne!(plan.files()[0].content_hash(), "");
        assert!(plan.unified_diff().contains("-fn old() {}"));
        assert!(plan.unified_diff().contains("+fn new() {}"));
        assert!(plan.safe_to_apply());
    }

    #[test]
    fn bounds_plans_and_keeps_project_lookup_isolated() -> Result<(), PlanStoreError> {
        let mut store = EditPlanStore::new(1, 1024, Duration::from_secs(60));
        let first = EditPlan::new(
            "project-a".to_string(),
            Vec::new(),
            Vec::new(),
            true,
            Duration::from_secs(60),
        );
        let first_id = first.id().clone();
        store.insert(first)?;

        assert!(store.get_for_project(&first_id, "project-b").is_err());
        assert!(store.get_for_project(&first_id, "project-a").is_ok());

        let second = EditPlan::new(
            "project-a".to_string(),
            Vec::new(),
            Vec::new(),
            true,
            Duration::from_secs(60),
        );
        let second_id = second.id().clone();
        store.insert(second)?;
        assert!(store.get(&first_id).is_none());
        assert!(store.get(&second_id).is_some());
        Ok(())
    }

    #[test]
    fn taking_a_plan_consumes_its_single_apply_token() -> Result<(), PlanStoreError> {
        let mut store = EditPlanStore::new(2, 1024, Duration::from_secs(60));
        let plan = EditPlan::new(
            "project-a".to_string(),
            Vec::new(),
            Vec::new(),
            true,
            Duration::from_secs(60),
        );
        let id = plan.id().clone();
        store.insert(plan)?;

        assert!(store.take_for_project(&id, "project-a").is_ok());
        assert!(matches!(
            store.take_for_project(&id, "project-a"),
            Err(PlanStoreError::NotFound(_))
        ));
        Ok(())
    }

    #[test]
    fn enforces_store_ttl_and_byte_limit() -> Result<(), PlanStoreError> {
        let mut expired = EditPlanStore::new(2, 1024, Duration::ZERO);
        let plan = EditPlan::new(
            "project-a".to_string(),
            Vec::new(),
            Vec::new(),
            true,
            Duration::from_secs(60),
        );
        let id = plan.id().clone();
        expired.insert(plan)?;
        assert!(expired.get(&id).is_none());

        let mut bounded = EditPlanStore::new(2, 1, Duration::from_secs(60));
        let large = EditPlan::new(
            "project-a".to_string(),
            Vec::new(),
            vec!["too large".to_string()],
            true,
            Duration::from_secs(60),
        );
        assert!(matches!(
            bounded.insert(large),
            Err(PlanStoreError::TooLarge { .. })
        ));
        Ok(())
    }

    #[test]
    fn rejects_stale_content_and_document_versions() {
        let snapshot = FileSnapshot::from_contents(
            PathBuf::from("src/lib.rs"),
            SnapshotSource::OpenDocument,
            Some(7),
            "before",
            "after",
        );

        assert!(matches!(
            snapshot.validate("changed", Some(7)),
            Err(SnapshotValidationError::ContentChanged { .. })
        ));
        assert!(matches!(
            snapshot.validate("before", Some(8)),
            Err(SnapshotValidationError::VersionChanged { expected: 7, .. })
        ));
        assert!(snapshot.validate("before", Some(7)).is_ok());
    }
}
