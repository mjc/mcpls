//! Lossless, write-free normalization of LSP `WorkspaceEdit` values.

use std::collections::HashMap;

use lsp_types::{
    ChangeAnnotation, CreateFileOptions, DeleteFileOptions, OneOf, RenameFileOptions, Uri,
    WorkspaceEdit,
};

/// A normalized workspace edit retaining operation order and all preconditions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedWorkspaceEdit {
    /// Operations in the order supplied by the LSP response.
    pub operations: Vec<EditOperation>,
    /// Change annotations referenced by normalized operations.
    pub change_annotations: Option<HashMap<String, ChangeAnnotation>>,
}

/// One lossless internal workspace-edit operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditOperation {
    /// Text changes against one document, optionally guarded by a version.
    Text {
        /// Target document URI.
        uri: Uri,
        /// Expected document version, if supplied by the server.
        version: Option<i32>,
        /// Edits for the target document.
        edits: Vec<NormalizedTextEdit>,
    },
    /// Create a file or directory.
    Create {
        /// Resource URI.
        uri: Uri,
        /// Resource options.
        options: Option<CreateFileOptions>,
        /// Optional change annotation identifier.
        annotation_id: Option<String>,
    },
    /// Rename a file or directory.
    Rename {
        /// Existing resource URI.
        old_uri: Uri,
        /// New resource URI.
        new_uri: Uri,
        /// Resource options.
        options: Option<RenameFileOptions>,
        /// Optional change annotation identifier.
        annotation_id: Option<String>,
    },
    /// Delete a file or directory.
    Delete {
        /// Resource URI.
        uri: Uri,
        /// Resource options.
        options: Option<DeleteFileOptions>,
        /// Optional change annotation identifier.
        annotation_id: Option<String>,
    },
}

/// A text edit retaining optional change annotation metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedTextEdit {
    /// Text range to replace.
    pub range: lsp_types::Range,
    /// Replacement text.
    pub new_text: String,
    /// Optional change annotation identifier.
    pub annotation_id: Option<String>,
}

/// Errors reserved for malformed future normalization inputs.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum NormalizationError {}

/// Normalize every LSP `WorkspaceEdit` representation without filesystem I/O.
///
/// # Errors
///
/// Returns a normalization error if a future protocol variant cannot be
/// represented by the internal operation list.
pub fn normalize(edit: WorkspaceEdit) -> Result<NormalizedWorkspaceEdit, NormalizationError> {
    let mut operations = Vec::new();

    if let Some(changes) = edit.changes {
        let mut changes: Vec<_> = changes.into_iter().collect();
        changes.sort_by_key(|(uri, _)| uri.to_string());
        operations.extend(changes.into_iter().map(|(uri, edits)| {
            EditOperation::Text {
                uri,
                version: None,
                edits: edits
                    .into_iter()
                    .map(|edit| normalize_text_edit(OneOf::Left(edit)))
                    .collect(),
            }
        }));
    }

    if let Some(document_changes) = edit.document_changes {
        match document_changes {
            lsp_types::DocumentChanges::Edits(edits) => {
                operations.extend(edits.into_iter().map(normalize_text_document_edit));
            }
            lsp_types::DocumentChanges::Operations(edits) => {
                operations.extend(edits.into_iter().map(normalize_operation));
            }
        }
    }

    Ok(NormalizedWorkspaceEdit {
        operations,
        change_annotations: edit.change_annotations,
    })
}

fn normalize_text_document_edit(edit: lsp_types::TextDocumentEdit) -> EditOperation {
    EditOperation::Text {
        uri: edit.text_document.uri,
        version: edit.text_document.version,
        edits: edit.edits.into_iter().map(normalize_text_edit).collect(),
    }
}

fn normalize_text_edit(
    edit: OneOf<lsp_types::TextEdit, lsp_types::AnnotatedTextEdit>,
) -> NormalizedTextEdit {
    match edit {
        OneOf::Left(edit) => NormalizedTextEdit {
            range: edit.range,
            new_text: edit.new_text,
            annotation_id: None,
        },
        OneOf::Right(edit) => NormalizedTextEdit {
            range: edit.text_edit.range,
            new_text: edit.text_edit.new_text,
            annotation_id: Some(edit.annotation_id),
        },
    }
}

fn normalize_operation(operation: lsp_types::DocumentChangeOperation) -> EditOperation {
    match operation {
        lsp_types::DocumentChangeOperation::Edit(edit) => normalize_text_document_edit(edit),
        lsp_types::DocumentChangeOperation::Op(operation) => match operation {
            lsp_types::ResourceOp::Create(operation) => EditOperation::Create {
                uri: operation.uri,
                options: operation.options,
                annotation_id: operation.annotation_id,
            },
            lsp_types::ResourceOp::Rename(operation) => EditOperation::Rename {
                old_uri: operation.old_uri,
                new_uri: operation.new_uri,
                options: operation.options,
                annotation_id: operation.annotation_id,
            },
            lsp_types::ResourceOp::Delete(operation) => {
                let annotation_id = operation
                    .options
                    .as_ref()
                    .and_then(|options| options.annotation_id.clone());
                EditOperation::Delete {
                    uri: operation.uri,
                    options: operation.options,
                    annotation_id,
                }
            }
        },
    }
}

#[cfg(test)]
#[path = "workspace_edit_tests.rs"]
mod tests;
