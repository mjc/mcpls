#![allow(clippy::mutable_key_type, clippy::unwrap_used)]
use std::collections::HashMap;
use std::str::FromStr;

use lsp_types::{
    DocumentChangeOperation, DocumentChanges, OneOf, Position, Range, ResourceOp, TextDocumentEdit,
    TextEdit, Uri, WorkspaceEdit,
};

use super::*;

#[test]
fn normalization_keeps_both_edit_encodings() {
    let uri = Uri::from_str("file:///workspace/src.rs").unwrap();
    let mut changes = HashMap::new();
    changes.insert(
        uri.clone(),
        vec![TextEdit::new(
            Range::new(Position::new(0, 0), Position::new(0, 0)),
            "map".to_string(),
        )],
    );
    let edit = WorkspaceEdit {
        changes: Some(changes),
        document_changes: Some(DocumentChanges::Edits(vec![TextDocumentEdit {
            text_document: lsp_types::OptionalVersionedTextDocumentIdentifier {
                uri,
                version: Some(4),
            },
            edits: vec![OneOf::Right(lsp_types::AnnotatedTextEdit {
                text_edit: TextEdit::new(
                    Range::new(Position::new(0, 0), Position::new(0, 0)),
                    "documentChanges".to_string(),
                ),
                annotation_id: "annotated".to_string(),
            })],
        }])),
        change_annotations: None,
    };

    let normalized = normalize(edit).unwrap();

    assert_eq!(normalized.operations.len(), 2);
    assert!(matches!(
        &normalized.operations[0],
        EditOperation::Text { version: None, .. }
    ));
    assert!(matches!(
        &normalized.operations[1],
        EditOperation::Text {
            version: Some(4),
            edits,
            ..
        } if edits[0].annotation_id.as_deref() == Some("annotated")
    ));
}

#[test]
fn normalization_preserves_resource_operation_order() {
    let create = ResourceOp::Create(lsp_types::CreateFile {
        uri: Uri::from_str("file:///workspace/new.rs").unwrap(),
        options: None,
        annotation_id: Some("create".to_string()),
    });
    let delete = ResourceOp::Delete(lsp_types::DeleteFile {
        uri: Uri::from_str("file:///workspace/old.rs").unwrap(),
        options: None,
    });
    let edit = WorkspaceEdit {
        changes: None,
        document_changes: Some(DocumentChanges::Operations(vec![
            DocumentChangeOperation::Op(create),
            DocumentChangeOperation::Op(delete),
        ])),
        change_annotations: None,
    };

    let normalized = normalize(edit).unwrap();

    assert!(matches!(
        &normalized.operations[0],
        EditOperation::Create { annotation_id: Some(id), .. } if id == "create"
    ));
    assert!(matches!(
        &normalized.operations[1],
        EditOperation::Delete { .. }
    ));
}
