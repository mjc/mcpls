//! Small AST-backed Rust refactors that feed the generic workspace-edit engine.

use std::fs;
use std::path::{Path, PathBuf};

use ast_grep_core::tree_sitter::LanguageExt;
use ast_grep_language::SupportLang;
use lsp_types::{
    CreateFile, CreateFileOptions, DocumentChangeOperation, DocumentChanges, OneOf,
    OptionalVersionedTextDocumentIdentifier, Position, ResourceOp, TextDocumentEdit, TextEdit, Uri,
    WorkspaceEdit,
};
use url::Url;

use crate::bridge::{EncodingConverter, PositionEncoding};

/// Errors raised while constructing an inline-module move preview.
#[derive(Debug, thiserror::Error)]
pub enum RustRefactorError {
    /// The source file could not be read.
    #[error("failed to read Rust source {path}: {source}")]
    Read {
        /// Source path.
        path: PathBuf,
        /// Filesystem error.
        #[source]
        source: std::io::Error,
    },
    /// The requested path or module is not supported by this refactor.
    #[error("{0}")]
    Invalid(String),
}

/// Build a `WorkspaceEdit` that moves one top-level inline Rust module to a file.
///
/// The edit creates the destination and replaces `mod name { ... }` with
/// `mod name;`. Existing destination conflicts are left to the normal preview
/// precondition checks.
///
/// # Errors
///
/// Returns [`RustRefactorError`] when the source cannot be read, the module is
/// not an inline top-level module, or a source position cannot be encoded.
#[allow(clippy::too_many_lines)]
pub fn move_inline_module_preview(
    source_path: &Path,
    module_name: &str,
    encoding: PositionEncoding,
) -> Result<WorkspaceEdit, RustRefactorError> {
    if source_path.extension().and_then(|ext| ext.to_str()) != Some("rs") {
        return Err(RustRefactorError::Invalid(
            "inline module moves require a Rust source file".to_string(),
        ));
    }
    if module_name.is_empty()
        || !module_name
            .chars()
            .all(|character| character == '_' || character.is_ascii_alphanumeric())
        || module_name
            .chars()
            .next()
            .is_some_and(|character| character.is_ascii_digit())
    {
        return Err(RustRefactorError::Invalid(format!(
            "invalid Rust module name: {module_name}"
        )));
    }

    let source = fs::read_to_string(source_path).map_err(|source| RustRefactorError::Read {
        path: source_path.to_path_buf(),
        source,
    })?;
    let tree = SupportLang::Rust.ast_grep(&source);
    let module = tree
        .root()
        .dfs()
        .find(|node| {
            node.kind() == "mod_item"
                && node
                    .field("name")
                    .is_some_and(|name| name.text() == module_name)
        })
        .ok_or_else(|| {
            RustRefactorError::Invalid(format!("inline Rust module not found: {module_name}"))
        })?;

    if module
        .parent()
        .is_some_and(|parent| parent.kind() == "mod_item")
    {
        return Err(RustRefactorError::Invalid(
            "nested inline module moves are not supported yet".to_string(),
        ));
    }
    let Some(body) = module.field("body") else {
        return Err(RustRefactorError::Invalid(format!(
            "module is already out-of-line: {module_name}"
        )));
    };
    let Some(name) = module.field("name") else {
        return Err(RustRefactorError::Invalid(format!(
            "module has no name: {module_name}"
        )));
    };
    let body_range = body.range();
    let name_range = name.range();
    let module_range = module.range();
    if body_range.end <= body_range.start + 1 || name_range.start < module_range.start {
        return Err(RustRefactorError::Invalid(
            "malformed inline module body".to_string(),
        ));
    }

    let destination = module_file_path(source_path, module_name)?;
    let source_uri = file_uri(source_path)?;
    let destination_uri = file_uri(&destination.path)?;
    let replacement_prefix = &source[module_range.start..name_range.start];
    let replacement = if destination.requires_path_attribute {
        format!("#[path = \"{module_name}.rs\"] {replacement_prefix}{module_name};")
    } else {
        format!("{replacement_prefix}{module_name};")
    };
    let content = source[body_range.start + 1..body_range.end - 1].to_string();
    let range = lsp_types::Range {
        start: byte_to_position(&source, module_range.start, encoding)?,
        end: byte_to_position(&source, module_range.end, encoding)?,
    };

    Ok(WorkspaceEdit {
        changes: None,
        document_changes: Some(DocumentChanges::Operations(vec![
            DocumentChangeOperation::Op(ResourceOp::Create(CreateFile {
                uri: destination_uri.clone(),
                options: Some(CreateFileOptions {
                    overwrite: Some(false),
                    ignore_if_exists: Some(false),
                }),
                annotation_id: None,
            })),
            DocumentChangeOperation::Edit(TextDocumentEdit {
                text_document: OptionalVersionedTextDocumentIdentifier {
                    uri: destination_uri,
                    version: None,
                },
                edits: vec![OneOf::Left(TextEdit {
                    range: lsp_types::Range::default(),
                    new_text: content,
                })],
            }),
            DocumentChangeOperation::Edit(TextDocumentEdit {
                text_document: OptionalVersionedTextDocumentIdentifier {
                    uri: source_uri,
                    version: None,
                },
                edits: vec![OneOf::Left(TextEdit {
                    range,
                    new_text: replacement,
                })],
            }),
        ])),
        change_annotations: None,
    })
}

struct ModuleDestination {
    path: PathBuf,
    requires_path_attribute: bool,
}

fn module_file_path(
    source_path: &Path,
    module_name: &str,
) -> Result<ModuleDestination, RustRefactorError> {
    let parent = source_path.parent().ok_or_else(|| {
        RustRefactorError::Invalid("Rust source has no parent directory".to_string())
    })?;
    let stem = source_path.file_stem().and_then(|stem| stem.to_str());
    let directory = match stem {
        Some("lib" | "main" | "mod") => parent.to_path_buf(),
        Some(stem) => parent.join(stem),
        None => {
            return Err(RustRefactorError::Invalid(
                "Rust source has no valid file stem".to_string(),
            ));
        }
    };
    if directory.is_dir() {
        Ok(ModuleDestination {
            path: directory.join(format!("{module_name}.rs")),
            requires_path_attribute: false,
        })
    } else {
        Ok(ModuleDestination {
            path: parent.join(format!("{module_name}.rs")),
            requires_path_attribute: true,
        })
    }
}

fn file_uri(path: &Path) -> Result<Uri, RustRefactorError> {
    let url = Url::from_file_path(path).map_err(|()| {
        RustRefactorError::Invalid(format!(
            "path cannot be represented as a file URI: {}",
            path.display()
        ))
    })?;
    url.as_str().parse().map_err(|error| {
        RustRefactorError::Invalid(format!("invalid file URI for {}: {error}", path.display()))
    })
}

fn byte_to_position(
    source: &str,
    byte_offset: usize,
    encoding: PositionEncoding,
) -> Result<Position, RustRefactorError> {
    let line = source[..byte_offset]
        .bytes()
        .filter(|byte| *byte == b'\n')
        .count();
    let line_start = source[..byte_offset]
        .rfind('\n')
        .map_or(0, |index| index + 1);
    let character = EncodingConverter::new(encoding)
        .byte_offset_to_character(&source[line_start..], byte_offset - line_start)
        .map_err(RustRefactorError::Invalid)?;
    Ok(Position {
        line: u32::try_from(line).map_err(|_| {
            RustRefactorError::Invalid("Rust source has too many lines".to_string())
        })?,
        character,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::bridge::{DocumentTracker, ResourceLimits};
    use crate::edit_apply::apply_plan;
    use crate::edit_preview::{PreviewLimits, preview_workspace_edit};

    #[test]
    fn builds_top_level_module_move_edit() {
        let root = tempfile::tempdir().unwrap();
        let source_path = root.path().join("lib.rs");
        fs::write(&source_path, "pub mod feature {\n    pub fn run() {}\n}\n").unwrap();

        let edit =
            move_inline_module_preview(&source_path, "feature", PositionEncoding::Utf8).unwrap();
        let DocumentChanges::Operations(operations) = edit.document_changes.unwrap() else {
            panic!("expected ordered resource and text operations");
        };
        assert!(matches!(
            operations[0],
            DocumentChangeOperation::Op(ResourceOp::Create(_))
        ));
        let DocumentChangeOperation::Edit(created) = &operations[1] else {
            panic!("expected destination file content edit");
        };
        let OneOf::Left(content_edit) = &created.edits[0] else {
            panic!("expected plain destination text edit");
        };
        assert_eq!(content_edit.new_text, "\n    pub fn run() {}\n");
        let DocumentChangeOperation::Edit(edit) = &operations[2] else {
            panic!("expected source text edit");
        };
        let OneOf::Left(text_edit) = &edit.edits[0] else {
            panic!("expected plain text edit");
        };
        assert_eq!(text_edit.new_text, "pub mod feature;");
    }

    #[test]
    fn rejects_out_of_line_module() {
        let root = tempfile::tempdir().unwrap();
        let source_path = root.path().join("lib.rs");
        fs::write(&source_path, "mod feature;\n").unwrap();

        let error = move_inline_module_preview(&source_path, "feature", PositionEncoding::Utf8)
            .unwrap_err();
        assert!(error.to_string().contains("already out-of-line"));
    }

    #[test]
    fn preserves_module_resolution_when_source_directory_is_missing() {
        let root = tempfile::tempdir().unwrap();
        let source_path = root.path().join("feature.rs");
        fs::write(&source_path, "mod child {\n    pub fn run() {}\n}\n").unwrap();

        let edit =
            move_inline_module_preview(&source_path, "child", PositionEncoding::Utf8).unwrap();
        let DocumentChanges::Operations(operations) = edit.document_changes.unwrap() else {
            panic!("expected ordered resource and text operations");
        };
        let DocumentChangeOperation::Edit(source_edit) = &operations[2] else {
            panic!("expected source text edit");
        };
        let OneOf::Left(text_edit) = &source_edit.edits[0] else {
            panic!("expected plain text edit");
        };
        assert_eq!(text_edit.new_text, "#[path = \"child.rs\"] mod child;");
    }

    #[test]
    fn previews_and_applies_the_module_move_through_the_generic_edit_engine() {
        let root = tempfile::tempdir().unwrap();
        let source_path = root.path().join("lib.rs");
        fs::write(&source_path, "pub mod feature {\n    pub fn run() {}\n}\n").unwrap();
        let boundary = crate::edit_paths::WorkspaceBoundary::new(root.path()).unwrap();
        let edit =
            move_inline_module_preview(&source_path, "feature", PositionEncoding::Utf8).unwrap();

        let artifact = preview_workspace_edit(
            &boundary,
            "project",
            edit,
            PositionEncoding::Utf8,
            &DocumentTracker::new(ResourceLimits::default(), HashMap::new()),
            PreviewLimits::default(),
        )
        .unwrap();
        assert!(artifact.plan.safe_to_apply());
        apply_plan(&boundary, &artifact.plan).unwrap();

        assert_eq!(
            fs::read_to_string(&source_path).unwrap(),
            "pub mod feature;\n"
        );
        assert_eq!(
            fs::read_to_string(root.path().join("feature.rs")).unwrap(),
            "\n    pub fn run() {}\n"
        );
    }
}
