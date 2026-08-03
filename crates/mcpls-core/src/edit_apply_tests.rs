#![allow(clippy::unwrap_used)]
use std::collections::HashMap;
use std::fs;
use std::time::Duration;

use tempfile::TempDir;

use super::*;
use crate::bridge::{DocumentTracker, ResourceLimits};
use crate::edit_paths::FileOperation;
use crate::edit_plan::{
    AuditFailureMode, AuditLogPolicy, EditAuditOutcome, EditPlan, FileSnapshot, SnapshotSource,
};

#[test]
fn applies_all_disk_snapshots_after_revalidating_preconditions() {
    let root = TempDir::new().unwrap();
    let first = root.path().join("first.rs");
    let second = root.path().join("second.rs");
    fs::write(&first, "first\n").unwrap();
    fs::write(&second, "second\n").unwrap();
    let plan = EditPlan::new(
        "project".to_string(),
        vec![
            FileSnapshot::from_contents(
                first.clone(),
                SnapshotSource::Disk,
                None,
                "first\n",
                "updated first\n",
            ),
            FileSnapshot::from_contents(
                second.clone(),
                SnapshotSource::Disk,
                None,
                "second\n",
                "updated second\n",
            ),
        ],
        Vec::new(),
        true,
        Duration::from_secs(60),
    );
    let boundary = WorkspaceBoundary::new(root.path()).unwrap();

    let report = apply_plan(&boundary, &plan).unwrap();

    assert_eq!(report.committed_files, vec![first, second]);
    assert_eq!(
        fs::read_to_string(root.path().join("first.rs")).unwrap(),
        "updated first\n"
    );
    assert_eq!(
        fs::read_to_string(root.path().join("second.rs")).unwrap(),
        "updated second\n"
    );
}

#[test]
fn rejects_one_stale_snapshot_before_committing_any_file() {
    let root = TempDir::new().unwrap();
    let first = root.path().join("first.rs");
    let second = root.path().join("second.rs");
    fs::write(&first, "first\n").unwrap();
    fs::write(&second, "second\n").unwrap();
    let plan = EditPlan::new(
        "project".to_string(),
        vec![
            FileSnapshot::from_contents(
                first.clone(),
                SnapshotSource::Disk,
                None,
                "first\n",
                "updated first\n",
            ),
            FileSnapshot::from_contents(
                second.clone(),
                SnapshotSource::Disk,
                None,
                "second\n",
                "updated second\n",
            ),
        ],
        Vec::new(),
        true,
        Duration::from_secs(60),
    );
    let boundary = WorkspaceBoundary::new(root.path()).unwrap();
    fs::write(&second, "changed after preview\n").unwrap();

    assert!(matches!(
        apply_plan(&boundary, &plan),
        Err(ApplyError::Stale(_))
    ));
    assert_eq!(fs::read_to_string(first).unwrap(), "first\n");
}

#[test]
fn rejects_open_document_snapshots_without_disk_writes() {
    let root = TempDir::new().unwrap();
    let file = root.path().join("open.rs");
    fs::write(&file, "disk\n").unwrap();
    let plan = EditPlan::new(
        "project".to_string(),
        vec![FileSnapshot::from_contents(
            file.clone(),
            SnapshotSource::OpenDocument,
            Some(3),
            "dirty\n",
            "updated\n",
        )],
        Vec::new(),
        true,
        Duration::from_secs(60),
    );
    let boundary = WorkspaceBoundary::new(root.path()).unwrap();

    assert!(matches!(
        apply_plan(&boundary, &plan),
        Err(ApplyError::UnsupportedSource(path)) if path == file
    ));
    assert_eq!(fs::read_to_string(file).unwrap(), "disk\n");
}

#[test]
fn applies_open_document_snapshot_against_tracked_content() {
    let root = TempDir::new().unwrap();
    let file = root.path().join("open.rs");
    fs::write(&file, "disk\n").unwrap();
    let mut documents = DocumentTracker::new(ResourceLimits::default(), HashMap::new());
    documents.open(file.clone(), "dirty\n".to_string()).unwrap();
    let plan = EditPlan::new(
        "project".to_string(),
        vec![FileSnapshot::from_contents(
            file.clone(),
            SnapshotSource::OpenDocument,
            Some(1),
            "dirty\n",
            "updated\n",
        )],
        Vec::new(),
        true,
        Duration::from_secs(60),
    );
    let boundary = WorkspaceBoundary::new(root.path()).unwrap();

    apply_plan_with_documents(&boundary, &plan, &documents).unwrap();

    assert_eq!(fs::read_to_string(file).unwrap(), "updated\n");
}

#[test]
fn applies_validated_resource_operations() {
    let root = TempDir::new().unwrap();
    let old = root.path().join("old.rs");
    let renamed = root.path().join("renamed.rs");
    fs::write(&old, "content\n").unwrap();
    let plan = EditPlan::new(
        "project".to_string(),
        Vec::new(),
        vec![format!("rename {} -> {}", old.display(), renamed.display())],
        true,
        Duration::from_secs(60),
    )
    .with_file_operations(vec![FileOperation::Rename {
        from: old.clone(),
        to: renamed.clone(),
        overwrite: false,
    }]);
    let boundary = WorkspaceBoundary::new(root.path()).unwrap();

    let report = apply_plan(&boundary, &plan).unwrap();

    assert_eq!(report.committed_files, vec![renamed.clone()]);
    assert!(!old.exists());
    assert_eq!(fs::read_to_string(renamed).unwrap(), "content\n");
}

#[test]
fn rename_without_overwrite_rechecks_destination_at_commit() {
    let root = TempDir::new().unwrap();
    let old = root.path().join("old.rs");
    let destination = root.path().join("destination.rs");
    fs::write(&old, "old\n").unwrap();
    fs::write(&destination, "destination\n").unwrap();
    let operation = ValidatedFileOperation::Rename {
        from: old.clone(),
        to: destination.clone(),
        overwrite: false,
    };

    let result = apply_resource_operation(&operation, &[]);

    assert!(matches!(
        result,
        Err(ApplyError::Operation(
            OperationValidationError::DestinationExists(path)
        )) if path == destination
    ));
    assert_eq!(fs::read_to_string(old).unwrap(), "old\n");
    assert_eq!(fs::read_to_string(destination).unwrap(), "destination\n");
}

#[test]
fn stored_application_consumes_the_plan_before_effects() {
    let root = TempDir::new().unwrap();
    let file = root.path().join("stored.rs");
    fs::write(&file, "before\n").unwrap();
    let plan = EditPlan::new(
        "project".to_string(),
        vec![FileSnapshot::from_contents(
            file.clone(),
            SnapshotSource::Disk,
            None,
            "before\n",
            "after\n",
        )],
        Vec::new(),
        true,
        Duration::from_secs(60),
    );
    let plan_id = plan.id().clone();
    let mut store = EditPlanStore::new(2, 1024, Duration::from_secs(60));
    store.insert(plan).unwrap();
    let boundary = WorkspaceBoundary::new(root.path()).unwrap();

    apply_stored_plan(&mut store, &boundary, "project", &plan_id).unwrap();

    assert_eq!(fs::read_to_string(&file).unwrap(), "after\n");
    assert!(matches!(
        apply_stored_plan(&mut store, &boundary, "project", &plan_id),
        Err(ApplyError::Store(PlanStoreError::NotFound(_)))
    ));
}

#[test]
fn stored_application_records_audit_outcome() {
    let root = TempDir::new().unwrap();
    let file = root.path().join("audited.rs");
    fs::write(&file, "before\n").unwrap();
    let plan = EditPlan::new(
        "project".to_string(),
        vec![FileSnapshot::from_contents(
            file,
            SnapshotSource::Disk,
            None,
            "before\n",
            "after\n",
        )],
        vec!["replace text".to_string()],
        true,
        Duration::from_secs(60),
    );
    let plan_id = plan.id().clone();
    let mut store = EditPlanStore::new(2, 1024, Duration::from_secs(60));
    store.insert(plan).unwrap();
    let boundary = WorkspaceBoundary::new(root.path()).unwrap();

    let report = apply_stored_plan(&mut store, &boundary, "project", &plan_id).unwrap();

    let records: Vec<_> = store.audit_records().collect();
    assert_eq!(records.len(), 1);
    let record = records[0];
    assert_eq!(record.project_id(), "project");
    assert_eq!(record.plan_id(), plan_id.as_str());
    assert_eq!(record.operations(), ["replace text"]);
    assert_eq!(record.precondition_hashes().len(), 1);
    assert_eq!(record.versions(), [None]);
    assert_eq!(record.committed_files(), report.committed_files.as_slice());
    assert_eq!(record.outcome(), &EditAuditOutcome::Committed);
    assert!(!record.rollback());
}

#[test]
fn stored_application_records_failed_audit_outcome() {
    let root = TempDir::new().unwrap();
    let file = root.path().join("stale.rs");
    fs::write(&file, "before\n").unwrap();
    let plan = EditPlan::new(
        "project".to_string(),
        vec![FileSnapshot::from_contents(
            file.clone(),
            SnapshotSource::Disk,
            None,
            "before\n",
            "after\n",
        )],
        Vec::new(),
        true,
        Duration::from_secs(60),
    );
    let plan_id = plan.id().clone();
    let mut store = EditPlanStore::new(2, 1024, Duration::from_secs(60));
    store.insert(plan).unwrap();
    fs::write(&file, "changed\n").unwrap();
    let boundary = WorkspaceBoundary::new(root.path()).unwrap();

    let error = apply_stored_plan(&mut store, &boundary, "project", &plan_id).unwrap_err();

    assert!(matches!(error, ApplyError::Stale(_)));
    let record = store.audit_records().next().unwrap();
    assert_eq!(
        record.outcome(),
        &EditAuditOutcome::Failed {
            error: error.to_string()
        }
    );
    assert!(!record.rollback());
    assert!(record.committed_files().is_empty());
}

#[test]
fn stored_application_persists_context_and_reports_closed_sink_failure() {
    let root = TempDir::new().unwrap();
    let file = root.path().join("context.rs");
    fs::write(&file, "before\n").unwrap();
    let plan = EditPlan::new(
        "project".to_string(),
        vec![FileSnapshot::from_contents(
            file.clone(),
            SnapshotSource::Disk,
            None,
            "before\n",
            "after\n",
        )],
        Vec::new(),
        true,
        Duration::from_secs(60),
    );
    let plan_id = plan.id().clone();
    let mut store = EditPlanStore::new(2, 1_024, Duration::from_secs(60));
    store.insert(plan).unwrap();
    let audit_path = root.path().join("audit.jsonl");
    store.set_audit_log(
        AuditLogPolicy::new(&audit_path, 4_096, AuditFailureMode::FailClosed).unwrap(),
    );
    let boundary = WorkspaceBoundary::new(root.path()).unwrap();

    apply_stored_plan_with_context(
        &mut store,
        &boundary,
        "project",
        &plan_id,
        Some("session-1".to_string()),
        Some("principal-1".to_string()),
    )
    .unwrap();

    let record = store.audit_records().next().unwrap();
    assert_eq!(record.session_id(), Some("session-1"));
    assert_eq!(record.principal(), Some("principal-1"));
    assert!(
        fs::read_to_string(audit_path)
            .unwrap()
            .contains("session-1")
    );

    store.set_audit_log(
        AuditLogPolicy::new(root.path(), 4_096, AuditFailureMode::FailClosed).unwrap(),
    );
    let failed_plan = EditPlan::new(
        "project".to_string(),
        vec![FileSnapshot::from_contents(
            file.clone(),
            SnapshotSource::Disk,
            None,
            "after\n",
            "final\n",
        )],
        Vec::new(),
        true,
        Duration::from_secs(60),
    );
    let failed_id = failed_plan.id().clone();
    store.insert(failed_plan).unwrap();
    let error =
        apply_stored_plan_with_context(&mut store, &boundary, "project", &failed_id, None, None)
            .unwrap_err();
    assert!(matches!(
        error,
        ApplyError::Audit(PlanStoreError::Audit { .. })
    ));
    assert_eq!(fs::read_to_string(file).unwrap(), "final\n");
}

#[test]
fn backup_failure_mode_controls_whether_writes_continue() {
    let root = TempDir::new().unwrap();
    let file = root.path().join("backup-mode.rs");
    let backup_path = root.path().join("backup-file");
    fs::write(&file, "before\n").unwrap();
    fs::write(&backup_path, "not a directory").unwrap();
    let boundary = WorkspaceBoundary::new(root.path()).unwrap();
    let plan = EditPlan::new(
        "project".to_string(),
        vec![FileSnapshot::from_contents(
            file.clone(),
            SnapshotSource::Disk,
            None,
            "before\n",
            "after\n",
        )],
        Vec::new(),
        true,
        Duration::from_secs(60),
    );

    let fail_closed = BackupPolicy::new(
        &boundary,
        &backup_path,
        1,
        1024,
        BackupFailureMode::FailClosed,
    )
    .unwrap();
    assert!(matches!(
        apply_plan_with_backup(&boundary, &plan, &fail_closed),
        Err(ApplyError::Backup(_))
    ));
    assert_eq!(fs::read_to_string(&file).unwrap(), "before\n");

    let fail_open = BackupPolicy::new(
        &boundary,
        &backup_path,
        1,
        1024,
        BackupFailureMode::FailOpen,
    )
    .unwrap();
    apply_plan_with_backup(&boundary, &plan, &fail_open).unwrap();
    assert_eq!(fs::read_to_string(&file).unwrap(), "after\n");
}
