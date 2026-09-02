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
    move_inline_module_preview_with_source(source_path, module_name, encoding, None, None)
}

/// Build an inline-module move from actor-owned source text when a document is open.
#[allow(clippy::too_many_lines)]
pub(crate) fn move_inline_module_preview_with_source(
    source_path: &Path,
    module_name: &str,
    encoding: PositionEncoding,
    source_override: Option<&str>,
    module_position: Option<Position>,
) -> Result<WorkspaceEdit, RustRefactorError> {
    if source_path.extension().and_then(|ext| ext.to_str()) != Some("rs") {
        return Err(RustRefactorError::Invalid(
            "inline module moves require a Rust source file".to_string(),
        ));
    }
    if !supported_module_name(module_name) {
        return Err(RustRefactorError::Invalid(format!(
            "invalid Rust module name: {module_name}"
        )));
    }

    let disk_source;
    let source = if let Some(source) = source_override {
        source
    } else {
        disk_source =
            fs::read_to_string(source_path).map_err(|source| RustRefactorError::Read {
                path: source_path.to_path_buf(),
                source,
            })?;
        &disk_source
    };
    let tree = SupportLang::Rust.ast_grep(source);
    let position_offset = module_position
        .map(|position| position_to_byte_offset(source, position, encoding))
        .transpose()?;
    let matching_modules = tree
        .root()
        .dfs()
        .filter(|node| {
            node.kind() == "mod_item"
                && node.field("name").is_some_and(|name| {
                    logical_module_name(name.text().as_ref()) == logical_module_name(module_name)
                        && position_offset.is_none_or(|offset| {
                            let range = node.range();
                            range.start <= offset && offset < range.end
                        })
                })
        })
        .collect::<Vec<_>>();
    if matching_modules.len() > 1 {
        return Err(RustRefactorError::Invalid(format!(
            "module name is ambiguous in Rust source: {module_name}"
        )));
    }
    let module = matching_modules.into_iter().next().ok_or_else(|| {
        RustRefactorError::Invalid(format!("inline Rust module not found: {module_name}"))
    })?;

    if has_mod_ancestor(&module) {
        return Err(RustRefactorError::Invalid(
            "nested inline module moves are not supported; select a top-level module".to_string(),
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
    let module_identifier = name.text();
    let module_file_name = logical_module_name(module_identifier.as_ref());
    let body_range = body.range();
    let name_range = name.range();
    let module_range = module.range();
    if body_range.end <= body_range.start + 1 || name_range.start < module_range.start {
        return Err(RustRefactorError::Invalid(
            "malformed inline module body".to_string(),
        ));
    }

    let destination = module_file_path(source_path, module_file_name)?;
    let source_uri = file_uri(source_path)?;
    let destination_uri = file_uri(&destination.path)?;
    let replacement_prefix = &source[module_range.start..name_range.start];
    let replacement = if destination.requires_path_attribute {
        format!("#[path = \"{module_file_name}.rs\"] {replacement_prefix}{module_identifier};")
    } else {
        format!("{replacement_prefix}{module_identifier};")
    };
    let content = source[body_range.start + 1..body_range.end - 1].to_string();
    if ["include!", "include_str!", "include_bytes!", "#[path"]
        .iter()
        .any(|needle| content.contains(needle))
    {
        return Err(RustRefactorError::Invalid(
            "inline module contains file-relative include or path syntax; move it manually"
                .to_string(),
        ));
    }
    let range = lsp_types::Range {
        start: byte_to_position(source, module_range.start, encoding)?,
        end: byte_to_position(source, module_range.end, encoding)?,
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

pub(crate) fn logical_module_name(name: &str) -> &str {
    name.strip_prefix("r#").unwrap_or(name)
}

fn supported_module_name(name: &str) -> bool {
    let logical = logical_module_name(name);
    let mut characters = logical.chars();
    let Some(first) = characters.next() else {
        return false;
    };
    (first == '_' || first.is_alphabetic())
        && characters.all(|character| character == '_' || character.is_alphanumeric())
}

fn has_mod_ancestor(
    node: &ast_grep_core::Node<'_, ast_grep_core::tree_sitter::StrDoc<SupportLang>>,
) -> bool {
    let mut parent = node.parent();
    while let Some(candidate) = parent {
        if candidate.kind() == "mod_item" {
            return true;
        }
        parent = candidate.parent();
    }
    false
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
    let Some(stem) = stem else {
        return Err(RustRefactorError::Invalid(
            "Rust source has no valid file stem".to_string(),
        ));
    };
    let module_directory = parent.join(stem);
    if module_directory.is_dir() {
        Ok(ModuleDestination {
            path: module_directory.join(format!("{module_name}.rs")),
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

fn position_to_byte_offset(
    source: &str,
    position: Position,
    encoding: PositionEncoding,
) -> Result<usize, RustRefactorError> {
    let line = usize::try_from(position.line)
        .map_err(|_| RustRefactorError::Invalid("module position line is too large".to_string()))?;
    let line_start = source
        .split_inclusive('\n')
        .take(line)
        .map(str::len)
        .sum::<usize>();
    let line_text = source
        .split_inclusive('\n')
        .nth(line)
        .ok_or_else(|| {
            RustRefactorError::Invalid("module position line is out of bounds".to_string())
        })?
        .strip_suffix('\n')
        .unwrap_or_else(|| source.split_inclusive('\n').nth(line).unwrap_or_default());
    let character_offset = EncodingConverter::new(encoding)
        .character_to_byte_offset(line_text, position.character)
        .map_err(RustRefactorError::Invalid)?;
    Ok(line_start + character_offset)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::edit_apply::apply_plan;
    use crate::edit_preview::{PreviewDocuments, PreviewLimits, preview_workspace_edit};

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
        assert_eq!(
            text_edit.new_text,
            "#[path = \"feature.rs\"] pub mod feature;"
        );
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
    fn supports_raw_and_unicode_module_names() {
        let root = tempfile::tempdir().unwrap();
        let source_path = root.path().join("lib.rs");
        fs::write(
            &source_path,
            "mod r#type { fn raw() {} }\nmod café { fn unicode() {} }\n",
        )
        .unwrap();

        let raw =
            move_inline_module_preview(&source_path, "r#type", PositionEncoding::Utf8).unwrap();
        let unicode =
            move_inline_module_preview(&source_path, "café", PositionEncoding::Utf8).unwrap();
        let DocumentChanges::Operations(raw_operations) = raw.document_changes.unwrap() else {
            panic!("expected ordered resource and text operations");
        };
        let DocumentChangeOperation::Edit(raw_source_edit) = &raw_operations[2] else {
            panic!("expected source text edit");
        };
        let OneOf::Left(raw_text_edit) = &raw_source_edit.edits[0] else {
            panic!("expected plain text edit");
        };
        assert!(raw_text_edit.new_text.contains("mod r#type;"));
        let DocumentChanges::Operations(unicode_operations) = unicode.document_changes.unwrap()
        else {
            panic!("expected ordered resource and text operations");
        };
        let DocumentChangeOperation::Edit(unicode_source_edit) = &unicode_operations[2] else {
            panic!("expected source text edit");
        };
        let OneOf::Left(unicode_text_edit) = &unicode_source_edit.edits[0] else {
            panic!("expected plain text edit");
        };
        assert!(unicode_text_edit.new_text.contains("mod café;"));
    }

    #[test]
    fn encodes_module_ranges_in_utf8_utf16_and_utf32() {
        let root = tempfile::tempdir().unwrap();
        let source_path = root.path().join("lib.rs");
        let source = "fn pre() {} /* 😀 */ mod feature {}\n";
        fs::write(&source_path, source).unwrap();
        let module_byte_offset = source.find("mod feature").unwrap();
        for encoding in [
            PositionEncoding::Utf8,
            PositionEncoding::Utf16,
            PositionEncoding::Utf32,
        ] {
            let character = EncodingConverter::new(encoding)
                .byte_offset_to_character(source, module_byte_offset)
                .unwrap();
            let edit = move_inline_module_preview_with_source(
                &source_path,
                "feature",
                encoding,
                Some(source),
                Some(Position { line: 0, character }),
            )
            .unwrap();
            let DocumentChanges::Operations(operations) = edit.document_changes.unwrap() else {
                panic!("expected ordered resource and text operations");
            };
            let DocumentChangeOperation::Edit(source_edit) = &operations[2] else {
                panic!("expected source text edit");
            };
            let OneOf::Left(text_edit) = &source_edit.edits[0] else {
                panic!("expected plain text edit");
            };
            assert_eq!(text_edit.range.start.character, character);
        }
    }

    #[test]
    fn rejects_file_relative_constructs_inside_the_moved_body() {
        let root = tempfile::tempdir().unwrap();
        let source_path = root.path().join("lib.rs");
        fs::write(
            &source_path,
            "mod feature { include!(\"generated.rs\"); }\n",
        )
        .unwrap();

        let error = move_inline_module_preview(&source_path, "feature", PositionEncoding::Utf8)
            .unwrap_err();
        assert!(error.to_string().contains("file-relative include or path"));
    }

    #[test]
    fn uses_an_explicit_path_for_a_non_root_lib_named_file() {
        let root = tempfile::tempdir().unwrap();
        let nested = root.path().join("nested");
        fs::create_dir_all(&nested).unwrap();
        let source_path = nested.join("lib.rs");
        fs::write(&source_path, "mod feature {}\n").unwrap();

        let edit =
            move_inline_module_preview(&source_path, "feature", PositionEncoding::Utf8).unwrap();
        let DocumentChanges::Operations(operations) = edit.document_changes.unwrap() else {
            panic!("expected ordered resource and text operations");
        };
        let DocumentChangeOperation::Edit(source_edit) = &operations[2] else {
            panic!("expected source text edit");
        };
        let OneOf::Left(text_edit) = &source_edit.edits[0] else {
            panic!("expected plain text edit");
        };
        assert!(text_edit.new_text.starts_with("#[path = \"feature.rs\"]"));
    }

    #[test]
    fn rejects_nested_inline_module_even_when_the_parent_is_a_declaration_list() {
        let root = tempfile::tempdir().unwrap();
        let source_path = root.path().join("lib.rs");
        fs::write(&source_path, "mod outer { mod child { fn run() {} } }\n").unwrap();

        let error =
            move_inline_module_preview(&source_path, "child", PositionEncoding::Utf8).unwrap_err();
        assert!(error.to_string().contains("nested inline module"));
    }

    #[test]
    fn rejects_ambiguous_top_level_module_names() {
        let root = tempfile::tempdir().unwrap();
        let source_path = root.path().join("lib.rs");
        fs::write(
            &source_path,
            "#[cfg(unix)] mod feature {}\n#[cfg(windows)] mod feature {}\n",
        )
        .unwrap();

        let error = move_inline_module_preview(&source_path, "feature", PositionEncoding::Utf8)
            .unwrap_err();
        assert!(error.to_string().contains("ambiguous"));
    }

    #[test]
    fn explicit_module_position_selects_one_of_duplicate_names() {
        let root = tempfile::tempdir().unwrap();
        let source_path = root.path().join("lib.rs");
        fs::write(
            &source_path,
            "#[cfg(unix)] mod feature { fn unix() {} }\n#[cfg(windows)] mod feature { fn windows() {} }\n",
        )
        .unwrap();

        let edit = move_inline_module_preview_with_source(
            &source_path,
            "feature",
            PositionEncoding::Utf8,
            None,
            Some(Position {
                line: 1,
                character: 16,
            }),
        )
        .unwrap();
        let DocumentChanges::Operations(operations) = edit.document_changes.unwrap() else {
            panic!("expected ordered resource and text operations");
        };
        let DocumentChangeOperation::Edit(created) = &operations[1] else {
            panic!("expected destination file content edit");
        };
        let OneOf::Left(content_edit) = &created.edits[0] else {
            panic!("expected plain destination text edit");
        };
        assert!(content_edit.new_text.contains("windows"));
    }

    #[test]
    fn uses_open_document_source_override_for_module_ranges_and_content() {
        let root = tempfile::tempdir().unwrap();
        let source_path = root.path().join("lib.rs");
        fs::write(&source_path, "pub mod feature { fn disk() {} }\n").unwrap();

        let edit = move_inline_module_preview_with_source(
            &source_path,
            "feature",
            PositionEncoding::Utf8,
            Some("// dirty\npub mod feature { fn open() {} }\n"),
            None,
        )
        .unwrap();
        let DocumentChanges::Operations(operations) = edit.document_changes.unwrap() else {
            panic!("expected ordered resource and text operations");
        };
        let DocumentChangeOperation::Edit(created) = &operations[1] else {
            panic!("expected destination file content edit");
        };
        let OneOf::Left(content_edit) = &created.edits[0] else {
            panic!("expected plain destination text edit");
        };
        assert!(content_edit.new_text.contains("open"));
        let DocumentChangeOperation::Edit(source_edit) = &operations[2] else {
            panic!("expected source text edit");
        };
        let OneOf::Left(text_edit) = &source_edit.edits[0] else {
            panic!("expected plain text edit");
        };
        assert_eq!(text_edit.range.start.line, 1);
    }

    #[test]
    fn preserves_attributes_comments_and_line_endings() {
        let root = tempfile::tempdir().unwrap();
        let source_path = root.path().join("lib.rs");
        fs::write(
            &source_path,
            "#[cfg(feature = \"feature\")]\r\npub(crate) mod feature {\r\n    #![allow(dead_code)]\r\n    // keep this comment\r\n    pub fn run() {}\r\n}\r\n",
        )
        .unwrap();

        let edit =
            move_inline_module_preview(&source_path, "feature", PositionEncoding::Utf8).unwrap();
        let DocumentChanges::Operations(operations) = edit.document_changes.unwrap() else {
            panic!("expected ordered resource and text operations");
        };
        let DocumentChangeOperation::Edit(created) = &operations[1] else {
            panic!("expected destination file content edit");
        };
        let OneOf::Left(content_edit) = &created.edits[0] else {
            panic!("expected plain destination text edit");
        };
        assert!(
            content_edit
                .new_text
                .contains("\r\n    #![allow(dead_code)]")
        );
        assert!(content_edit.new_text.contains("// keep this comment"));
        let DocumentChangeOperation::Edit(source_edit) = &operations[2] else {
            panic!("expected source text edit");
        };
        let OneOf::Left(text_edit) = &source_edit.edits[0] else {
            panic!("expected plain text edit");
        };
        assert!(text_edit.new_text.contains("pub(crate) mod feature;"));
    }

    #[test]
    fn reports_an_existing_destination_conflict_during_preview() {
        let root = tempfile::tempdir().unwrap();
        let source_path = root.path().join("lib.rs");
        let destination_path = root.path().join("feature.rs");
        fs::write(&source_path, "mod feature { fn run() {} }\n").unwrap();
        fs::write(&destination_path, "// existing\n").unwrap();
        let boundary = crate::edit_paths::WorkspaceBoundary::new(root.path()).unwrap();
        let edit =
            move_inline_module_preview(&source_path, "feature", PositionEncoding::Utf8).unwrap();

        let error = preview_workspace_edit(
            &boundary,
            "project",
            edit,
            PositionEncoding::Utf8,
            &PreviewDocuments::default(),
            PreviewLimits::default(),
        )
        .unwrap_err();

        assert!(error.to_string().contains("destination already exists"));
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
            &PreviewDocuments::default(),
            PreviewLimits::default(),
        )
        .unwrap();
        assert!(artifact.plan.safe_to_apply());
        apply_plan(&boundary, &artifact.plan).unwrap();

        assert_eq!(
            fs::read_to_string(&source_path).unwrap(),
            "#[path = \"feature.rs\"] pub mod feature;\n"
        );
        assert_eq!(
            fs::read_to_string(root.path().join("feature.rs")).unwrap(),
            "\n    pub fn run() {}\n"
        );
    }
}
