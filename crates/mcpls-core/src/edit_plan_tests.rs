#![allow(clippy::unwrap_used)]

use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use tempfile::TempDir;

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
fn policy_changes_invalidate_outstanding_plans() -> Result<(), PlanStoreError> {
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

    store.update_policy(EditPolicy::new(EditMode::Refactor));

    assert!(matches!(
        store.take_for_project(&id, "project-a"),
        Err(PlanStoreError::PolicyChanged { plan_id, .. }) if plan_id == id
    ));
    assert!(store.get(&id).is_none());
    Ok(())
}

#[test]
fn expired_and_evicted_plans_have_specific_failures() -> Result<(), PlanStoreError> {
    let mut expired = EditPlanStore::new(2, 1024, Duration::ZERO);
    let expired_plan = EditPlan::new(
        "project-a".to_string(),
        Vec::new(),
        Vec::new(),
        true,
        Duration::from_secs(60),
    );
    let expired_id = expired_plan.id().clone();
    expired.insert(expired_plan)?;
    assert!(matches!(
        expired.take_for_project(&expired_id, "project-a"),
        Err(PlanStoreError::Expired(id)) if id == expired_id
    ));

    let mut bounded = EditPlanStore::new(1, 1024, Duration::from_secs(60));
    let first = EditPlan::new(
        "project-a".to_string(),
        Vec::new(),
        Vec::new(),
        true,
        Duration::from_secs(60),
    );
    let first_id = first.id().clone();
    bounded.insert(first)?;
    bounded.insert(EditPlan::new(
        "project-a".to_string(),
        Vec::new(),
        Vec::new(),
        true,
        Duration::from_secs(60),
    ))?;
    assert!(matches!(
        bounded.take_for_project(&first_id, "project-a"),
        Err(PlanStoreError::Evicted(id)) if id == first_id
    ));
    Ok(())
}

#[test]
fn durable_audit_records_keep_context_without_source_content() {
    let root = TempDir::new().unwrap();
    let path = root.path().join("audit.jsonl");
    let policy = AuditLogPolicy::new(&path, 4_096, AuditFailureMode::FailClosed).unwrap();
    let plan = EditPlan::new(
        "project-a".to_string(),
        vec![FileSnapshot::from_contents(
            PathBuf::from("src/lib.rs"),
            SnapshotSource::Disk,
            None,
            "secret source\n",
            "updated\n",
        )],
        vec!["replace text".to_string()],
        true,
        Duration::from_secs(60),
    );
    let record = EditAuditRecord::for_plan_with_context(
        &plan,
        Some("session-a".to_string()),
        Some("principal-a".to_string()),
    )
    .committed(vec![PathBuf::from("src/lib.rs")]);
    let mut store = EditPlanStore::new(2, 1_024, Duration::from_secs(60));
    store.set_audit_log(policy);

    store.record_audit_with_policy(record).unwrap();

    let line = fs::read_to_string(path).unwrap();
    assert!(line.contains("\"session_id\":\"session-a\""));
    assert!(line.contains("\"principal\":\"principal-a\""));
    assert!(line.contains("\"precondition_hashes\""));
    assert!(!line.contains("secret source"));
    assert_eq!(store.audit_records().count(), 1);
}

#[test]
fn audit_sink_failure_mode_controls_memory_fallback() {
    let root = TempDir::new().unwrap();
    let plan = EditPlan::new(
        "project-a".to_string(),
        Vec::new(),
        Vec::new(),
        true,
        Duration::from_secs(60),
    );
    let record = EditAuditRecord::for_plan(&plan);

    let mut fail_closed = EditPlanStore::new(2, 1_024, Duration::from_secs(60));
    fail_closed.set_audit_log(
        AuditLogPolicy::new(root.path(), 4_096, AuditFailureMode::FailClosed).unwrap(),
    );
    assert!(matches!(
        fail_closed.record_audit_with_policy(record.clone()),
        Err(PlanStoreError::Audit { .. })
    ));
    assert_eq!(fail_closed.audit_records().count(), 0);

    let mut fail_open = EditPlanStore::new(2, 1_024, Duration::from_secs(60));
    fail_open.set_audit_log(
        AuditLogPolicy::new(root.path(), 4_096, AuditFailureMode::FailOpen).unwrap(),
    );
    fail_open.record_audit_with_policy(record).unwrap();
    assert_eq!(fail_open.audit_records().count(), 1);
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
