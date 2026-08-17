#![allow(clippy::unwrap_used)]
use std::collections::HashMap;

use tempfile::TempDir;

use super::*;
use crate::bridge::path_to_uri;
use crate::workspace_edit::NormalizedTextEdit;

#[test]
fn previews_disk_text_edit_without_writing() {
    let root = TempDir::new().unwrap();
    let file = root.path().join("src.rs");
    fs::write(&file, "before\n").unwrap();
    let boundary = WorkspaceBoundary::new(root.path()).unwrap();
    let edit = WorkspaceEdit {
        changes: Some(HashMap::from([(
            path_to_uri(&file).unwrap(),
            vec![lsp_types::TextEdit {
                range: lsp_types::Range::new(
                    lsp_types::Position::new(0, 0),
                    lsp_types::Position::new(0, 6),
                ),
                new_text: "after".to_string(),
            }],
        )])),
        document_changes: None,
        change_annotations: None,
    };

    let artifact = preview_workspace_edit(
        &boundary,
        "project",
        edit,
        PositionEncoding::Utf8,
        &DocumentTracker::new(crate::bridge::ResourceLimits::default(), HashMap::new()),
        PreviewLimits::default(),
    )
    .unwrap();
    assert!(artifact.plan.safe_to_apply());
    assert!(artifact.plan.unified_diff().contains("+after"));
    assert_eq!(fs::read_to_string(file).unwrap(), "before\n");
}

#[test]
fn preview_and_apply_preserve_rust_literal_bytes_across_workspace_edit_transport() {
    let root = TempDir::new().unwrap();
    let file = root.path().join("src.rs");
    fs::write(&file, "before\n").unwrap();
    let boundary = WorkspaceBoundary::new(root.path()).unwrap();
    let expected = r##"let quote = "\"";
let slash = '\\';
let path = "C:\\tmp";
let raw = r#"quoted \" text"#;
let multiline = "first
second";
"##;
    let edit = WorkspaceEdit {
        changes: Some(HashMap::from([(
            path_to_uri(&file).unwrap(),
            vec![lsp_types::TextEdit {
                range: lsp_types::Range::new(
                    lsp_types::Position::new(0, 0),
                    lsp_types::Position::new(1, 0),
                ),
                new_text: expected.to_string(),
            }],
        )])),
        document_changes: None,
        change_annotations: None,
    };
    let edit: WorkspaceEdit = serde_json::from_value(serde_json::to_value(edit).unwrap()).unwrap();

    let artifact = preview_workspace_edit(
        &boundary,
        "project",
        edit,
        PositionEncoding::Utf8,
        &DocumentTracker::new(crate::bridge::ResourceLimits::default(), HashMap::new()),
        PreviewLimits::default(),
    )
    .unwrap();
    assert!(artifact.plan.safe_to_apply());
    assert_eq!(artifact.plan.files()[0].planned_content(), expected);

    crate::edit_apply::apply_plan(&boundary, &artifact.plan).unwrap();
    assert_eq!(fs::read_to_string(file).unwrap(), expected);
}

#[test]
fn previews_text_edit_after_ordered_create() {
    let root = TempDir::new().unwrap();
    let file = root.path().join("created.rs");
    let boundary = WorkspaceBoundary::new(root.path()).unwrap();
    let edit = NormalizedWorkspaceEdit {
        operations: vec![
            EditOperation::Create {
                uri: path_to_uri(&file).unwrap(),
                options: None,
                annotation_id: None,
            },
            EditOperation::Text {
                uri: path_to_uri(&file).unwrap(),
                version: None,
                edits: vec![NormalizedTextEdit {
                    range: lsp_types::Range::default(),
                    new_text: "created content\n".to_string(),
                    annotation_id: None,
                }],
            },
        ],
        change_annotations: None,
    };

    let artifact = preview_normalized(
        &boundary,
        "project",
        edit,
        PositionEncoding::Utf8,
        &DocumentTracker::new(crate::bridge::ResourceLimits::default(), HashMap::new()),
        PreviewLimits::default(),
    )
    .unwrap();

    assert!(artifact.plan.safe_to_apply());
    assert_eq!(artifact.plan.files()[0].original_content(), "");
    assert_eq!(
        artifact.plan.files()[0].planned_content(),
        "created content\n"
    );
}

#[test]
fn rejects_text_before_create_in_the_same_transaction() {
    let root = TempDir::new().unwrap();
    let file = root.path().join("existing.rs");
    fs::write(&file, "before\n").unwrap();
    let boundary = WorkspaceBoundary::new(root.path()).unwrap();
    let edit = NormalizedWorkspaceEdit {
        operations: vec![
            EditOperation::Text {
                uri: path_to_uri(&file).unwrap(),
                version: None,
                edits: vec![NormalizedTextEdit {
                    range: lsp_types::Range::new(
                        lsp_types::Position::new(0, 0),
                        lsp_types::Position::new(0, 6),
                    ),
                    new_text: "edited".to_string(),
                    annotation_id: None,
                }],
            },
            EditOperation::Create {
                uri: path_to_uri(&file).unwrap(),
                options: Some(lsp_types::CreateFileOptions {
                    overwrite: Some(true),
                    ignore_if_exists: Some(false),
                }),
                annotation_id: None,
            },
        ],
        change_annotations: None,
    };

    let artifact = preview_normalized(
        &boundary,
        "project",
        edit,
        PositionEncoding::Utf8,
        &DocumentTracker::new(crate::bridge::ResourceLimits::default(), HashMap::new()),
        PreviewLimits::default(),
    )
    .unwrap();

    assert!(!artifact.plan.safe_to_apply());
    assert!(
        artifact
            .conflicts
            .iter()
            .any(|conflict| conflict.contains("follows text edits"))
    );
}

#[test]
fn create_overwrite_then_text_retains_existing_preimage() {
    let root = TempDir::new().unwrap();
    let file = root.path().join("existing.rs");
    fs::write(&file, "before\n").unwrap();
    let boundary = WorkspaceBoundary::new(root.path()).unwrap();
    let edit = NormalizedWorkspaceEdit {
        operations: vec![
            EditOperation::Create {
                uri: path_to_uri(&file).unwrap(),
                options: Some(lsp_types::CreateFileOptions {
                    overwrite: Some(true),
                    ignore_if_exists: Some(false),
                }),
                annotation_id: None,
            },
            EditOperation::Text {
                uri: path_to_uri(&file).unwrap(),
                version: None,
                edits: vec![NormalizedTextEdit {
                    range: lsp_types::Range::default(),
                    new_text: "replacement\n".to_string(),
                    annotation_id: None,
                }],
            },
        ],
        change_annotations: None,
    };

    let artifact = preview_normalized(
        &boundary,
        "project",
        edit,
        PositionEncoding::Utf8,
        &DocumentTracker::new(crate::bridge::ResourceLimits::default(), HashMap::new()),
        PreviewLimits::default(),
    )
    .unwrap();

    assert!(artifact.plan.safe_to_apply());
    assert_eq!(artifact.plan.files()[0].original_content(), "before\n");
    assert!(!artifact.plan.files()[0].was_created());
    assert_eq!(artifact.plan.files()[0].planned_content(), "replacement\n");
}

#[test]
fn rejects_resource_operation_limit() {
    let root = TempDir::new().unwrap();
    let boundary = WorkspaceBoundary::new(root.path()).unwrap();
    let path = root.path().join("created.rs");
    let edit = NormalizedWorkspaceEdit {
        operations: vec![EditOperation::Create {
            uri: path_to_uri(&path).unwrap(),
            options: None,
            annotation_id: None,
        }],
        change_annotations: None,
    };
    let limits = PreviewLimits {
        max_resource_operations: 0,
        ..PreviewLimits::default()
    };

    let result = preview_normalized(
        &boundary,
        "project",
        edit,
        PositionEncoding::Utf8,
        &DocumentTracker::new(crate::bridge::ResourceLimits::default(), HashMap::new()),
        limits,
    );

    assert!(matches!(
        result,
        Err(PreviewError::Limit {
            kind: "resource operation",
            actual: 1,
            limit: 0,
        })
    ));
}

#[test]
fn permits_text_edits_followed_by_one_rename() {
    let root = TempDir::new().unwrap();
    let source = root.path().join("source.rs");
    let reference = root.path().join("lib.rs");
    let destination = root.path().join("renamed.rs");
    fs::write(&source, "pub fn value() {}\n").unwrap();
    fs::write(&reference, "mod source;\n").unwrap();
    let boundary = WorkspaceBoundary::new(root.path()).unwrap();
    let edit = NormalizedWorkspaceEdit {
        operations: vec![
            EditOperation::Text {
                uri: path_to_uri(&reference).unwrap(),
                version: None,
                edits: vec![NormalizedTextEdit {
                    range: lsp_types::Range::new(
                        lsp_types::Position::new(0, 4),
                        lsp_types::Position::new(0, 10),
                    ),
                    new_text: "renamed".to_string(),
                    annotation_id: None,
                }],
            },
            EditOperation::Rename {
                old_uri: path_to_uri(&source).unwrap(),
                new_uri: path_to_uri(&destination).unwrap(),
                options: None,
                annotation_id: None,
            },
        ],
        change_annotations: None,
    };

    let artifact = preview_normalized(
        &boundary,
        "project",
        edit,
        PositionEncoding::Utf8,
        &DocumentTracker::new(crate::bridge::ResourceLimits::default(), HashMap::new()),
        PreviewLimits::default(),
    )
    .unwrap();

    assert!(artifact.plan.safe_to_apply(), "{:?}", artifact.conflicts);
    assert_eq!(artifact.plan.files()[0].planned_content(), "mod renamed;\n");
    assert!(matches!(
        artifact.plan.file_operations(),
        [FileOperation::Rename { from, to, .. }] if from == &source && to == &destination
    ));
}

#[test]
fn rejects_text_edits_that_follow_a_rename() {
    let root = TempDir::new().unwrap();
    let source = root.path().join("source.rs");
    let reference = root.path().join("lib.rs");
    let destination = root.path().join("renamed.rs");
    fs::write(&source, "pub fn value() {}\n").unwrap();
    fs::write(&reference, "mod source;\n").unwrap();
    let boundary = WorkspaceBoundary::new(root.path()).unwrap();
    let edit = NormalizedWorkspaceEdit {
        operations: vec![
            EditOperation::Rename {
                old_uri: path_to_uri(&source).unwrap(),
                new_uri: path_to_uri(&destination).unwrap(),
                options: None,
                annotation_id: None,
            },
            EditOperation::Text {
                uri: path_to_uri(&reference).unwrap(),
                version: None,
                edits: vec![],
            },
        ],
        change_annotations: None,
    };

    let artifact = preview_normalized(
        &boundary,
        "project",
        edit,
        PositionEncoding::Utf8,
        &DocumentTracker::new(crate::bridge::ResourceLimits::default(), HashMap::new()),
        PreviewLimits::default(),
    )
    .unwrap();

    assert!(!artifact.plan.safe_to_apply());
    assert!(
        artifact
            .conflicts
            .iter()
            .any(|conflict| conflict.contains("follow a rename"))
    );
}

#[test]
fn rejects_per_file_byte_limit() {
    let root = TempDir::new().unwrap();
    let file = root.path().join("large.rs");
    fs::write(&file, "before\n").unwrap();
    let boundary = WorkspaceBoundary::new(root.path()).unwrap();
    let edit = NormalizedWorkspaceEdit {
        operations: vec![EditOperation::Text {
            uri: path_to_uri(&file).unwrap(),
            version: None,
            edits: vec![NormalizedTextEdit {
                range: lsp_types::Range::new(
                    lsp_types::Position::new(0, 0),
                    lsp_types::Position::new(0, 6),
                ),
                new_text: "after".to_string(),
                annotation_id: None,
            }],
        }],
        change_annotations: None,
    };
    let limits = PreviewLimits {
        max_file_bytes: 10,
        ..PreviewLimits::default()
    };

    let result = preview_normalized(
        &boundary,
        "project",
        edit,
        PositionEncoding::Utf8,
        &DocumentTracker::new(crate::bridge::ResourceLimits::default(), HashMap::new()),
        limits,
    );

    assert!(matches!(
        result,
        Err(PreviewError::Limit {
            kind: "file byte",
            actual: 13,
            limit: 10,
        })
    ));
}
