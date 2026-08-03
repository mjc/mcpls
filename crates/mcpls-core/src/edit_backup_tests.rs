#![allow(clippy::unwrap_used)]
use std::fs;
use std::time::Duration;

use tempfile::TempDir;

use super::*;
use crate::edit_paths::{FileOperation, WorkspaceBoundary};
use crate::edit_plan::{EditPlan, FileSnapshot, SnapshotSource};

#[test]
fn backup_policy_rejects_a_root_outside_the_workspace() {
    let workspace = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    let boundary = WorkspaceBoundary::new(workspace.path()).unwrap();

    assert!(matches!(
        BackupPolicy::new(
            &boundary,
            outside.path().join("backups"),
            2,
            1024,
            BackupFailureMode::FailClosed,
        ),
        Err(BackupError::OutsideWorkspace { .. })
    ));
}

#[test]
fn backup_archives_restore_and_retain_bounded_entries() {
    let workspace = TempDir::new().unwrap();
    let backup_root = workspace.path().join(".mcpls-backups");
    fs::create_dir(&backup_root).unwrap();
    let file = workspace.path().join("src.rs");
    fs::write(&file, "before\n").unwrap();
    let boundary = WorkspaceBoundary::new(workspace.path()).unwrap();
    let policy = BackupPolicy::new(
        &boundary,
        &backup_root,
        1,
        1024 * 1024,
        BackupFailureMode::FailClosed,
    )
    .unwrap();
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

    let first = BackupArchive::create(&policy, &boundary, &plan).unwrap();
    fs::write(&file, "changed\n").unwrap();
    first.restore(&boundary).unwrap();
    assert_eq!(fs::read_to_string(&file).unwrap(), "before\n");

    let second_plan = EditPlan::new(
        "project".to_string(),
        vec![FileSnapshot::from_contents(
            file,
            SnapshotSource::Disk,
            None,
            "before\n",
            "again\n",
        )],
        Vec::new(),
        true,
        Duration::from_secs(60),
    );
    BackupArchive::create(&policy, &boundary, &second_plan).unwrap();
    let archives = fs::read_dir(&backup_root).unwrap().count();
    assert_eq!(archives, 1);
}

#[test]
fn backup_archives_restore_resource_operations() {
    let root = TempDir::new().unwrap();
    let old = root.path().join("old.rs");
    let renamed = root.path().join("renamed.rs");
    let created = root.path().join("created.rs");
    fs::write(&old, "old\n").unwrap();
    let boundary = WorkspaceBoundary::new(root.path()).unwrap();
    let plan = EditPlan::new(
        "project".to_string(),
        Vec::new(),
        Vec::new(),
        true,
        Duration::from_secs(60),
    )
    .with_file_operations(vec![
        FileOperation::Rename {
            from: old.clone(),
            to: renamed.clone(),
            overwrite: false,
        },
        FileOperation::Create {
            path: created.clone(),
            overwrite: false,
        },
    ]);
    let policy = BackupPolicy::new(
        &boundary,
        root.path().join("backups"),
        2,
        4_096,
        BackupFailureMode::FailClosed,
    )
    .unwrap();
    let archive = BackupArchive::create(&policy, &boundary, &plan).unwrap();

    fs::rename(&old, &renamed).unwrap();
    fs::write(&created, []).unwrap();
    archive.restore(&boundary).unwrap();

    assert_eq!(fs::read_to_string(&old).unwrap(), "old\n");
    assert!(!renamed.exists());
    assert!(!created.exists());
}
