#![allow(clippy::unwrap_used)]
use std::fs;

use tempfile::TempDir;

use super::*;

#[test]
fn validates_existing_files_and_rejects_escape_targets() {
    let root = TempDir::new().unwrap();
    let file = root.path().join("src.rs");
    fs::write(&file, "fn main() {}\n").unwrap();
    let boundary = WorkspaceBoundary::new(root.path()).unwrap();

    assert_eq!(
        boundary.validate_existing(&file).unwrap(),
        file.canonicalize().unwrap()
    );
    assert!(matches!(
        boundary.validate_target(root.path().join("../escape.rs")),
        Err(PathSafetyError::OutsideWorkspace { .. })
    ));
}

#[cfg(unix)]
#[test]
fn rejects_symlink_escape_for_existing_and_target_paths() {
    use std::os::unix::fs::symlink;

    let root = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    let outside_file = outside.path().join("secret.rs");
    fs::write(&outside_file, "secret").unwrap();
    let link = root.path().join("link");
    symlink(outside.path(), &link).unwrap();
    let boundary = WorkspaceBoundary::new(root.path()).unwrap();

    assert!(matches!(
        boundary.validate_existing(link.join("secret.rs")),
        Err(PathSafetyError::OutsideWorkspace { .. })
    ));
    assert!(matches!(
        boundary.validate_target(link.join("new.rs")),
        Err(PathSafetyError::OutsideWorkspace { .. })
    ));
}

#[test]
fn validates_operation_preconditions_after_containment() {
    let root = TempDir::new().unwrap();
    let source = root.path().join("source.rs");
    let existing = root.path().join("existing.rs");
    fs::write(&source, "source").unwrap();
    fs::write(&existing, "existing").unwrap();
    let boundary = WorkspaceBoundary::new(root.path()).unwrap();

    assert!(matches!(
        boundary.validate_operation(&FileOperation::Create {
            path: existing.clone(),
            overwrite: false,
        }),
        Err(OperationValidationError::DestinationExists(path)) if path == existing
    ));
    assert!(matches!(
        boundary.validate_operation(&FileOperation::Rename {
            from: source.clone(),
            to: existing.clone(),
            overwrite: true,
        }),
        Ok(ValidatedFileOperation::Rename { from, to, .. })
            if from == source && to == existing
    ));
}

#[test]
fn preserves_nested_missing_target_components() {
    let root = TempDir::new().unwrap();
    let boundary = WorkspaceBoundary::new(root.path()).unwrap();
    let target = root.path().join("nested").join("new.txt");

    assert_eq!(boundary.validate_target(&target).unwrap(), target);
}

#[test]
fn requires_recursive_directory_deletes() {
    let root = TempDir::new().unwrap();
    let directory = root.path().join("nested");
    fs::create_dir(&directory).unwrap();
    let boundary = WorkspaceBoundary::new(root.path()).unwrap();

    assert!(matches!(
        boundary.validate_operation(&FileOperation::Delete {
            path: directory.clone(),
            recursive: false,
        }),
        Err(OperationValidationError::RecursiveDeleteRequired(path)) if path == directory
    ));
}

#[test]
fn validates_operation_batches_in_input_order() {
    let root = TempDir::new().unwrap();
    let first = root.path().join("first.rs");
    let second = root.path().join("second.rs");
    fs::write(&first, "first").unwrap();
    fs::write(&second, "second").unwrap();
    let boundary = WorkspaceBoundary::new(root.path()).unwrap();
    let operations = [
        FileOperation::Rename {
            from: first.clone(),
            to: root.path().join("renamed.rs"),
            overwrite: false,
        },
        FileOperation::Delete {
            path: second.clone(),
            recursive: false,
        },
    ];

    let validated = boundary.validate_operations(&operations).unwrap();

    assert!(matches!(
        &validated[..],
        [
            ValidatedFileOperation::Rename { from, .. },
            ValidatedFileOperation::Delete { path, .. }
        ] if from == &first && path == &second
    ));
}
