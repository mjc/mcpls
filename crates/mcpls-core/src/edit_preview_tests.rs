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
            path_to_uri(&file),
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
fn rejects_resource_operation_limit() {
    let root = TempDir::new().unwrap();
    let boundary = WorkspaceBoundary::new(root.path()).unwrap();
    let path = root.path().join("created.rs");
    let edit = NormalizedWorkspaceEdit {
        operations: vec![EditOperation::Create {
            uri: path_to_uri(&path),
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
fn rejects_per_file_byte_limit() {
    let root = TempDir::new().unwrap();
    let file = root.path().join("large.rs");
    fs::write(&file, "before\n").unwrap();
    let boundary = WorkspaceBoundary::new(root.path()).unwrap();
    let edit = NormalizedWorkspaceEdit {
        operations: vec![EditOperation::Text {
            uri: path_to_uri(&file),
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
