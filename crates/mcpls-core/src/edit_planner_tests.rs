use lsp_types::{Position, Range};

use crate::bridge::PositionEncoding;
use crate::workspace_edit::NormalizedTextEdit;

use super::*;

fn edit(start: Position, end: Position, text: &str) -> NormalizedTextEdit {
    NormalizedTextEdit {
        range: Range::new(start, end),
        new_text: text.to_string(),
        annotation_id: None,
    }
}

#[test]
fn applies_multiple_utf16_edits_in_reverse_byte_order() -> Result<(), EditPlanError> {
    let edits = [
        edit(Position::new(0, 0), Position::new(0, 1), "A"),
        edit(Position::new(0, 4), Position::new(0, 6), "B"),
    ];

    assert_eq!(
        apply_text_edits("one 😀\n", &edits, PositionEncoding::Utf16)?,
        "Ane B\n"
    );
    Ok(())
}

#[test]
fn rejects_overlapping_edits() {
    let edits = [
        edit(Position::new(0, 0), Position::new(0, 2), "A"),
        edit(Position::new(0, 1), Position::new(0, 3), "B"),
    ];

    assert!(matches!(
        apply_text_edits("abcd", &edits, PositionEncoding::Utf8),
        Err(EditPlanError::Overlapping { .. })
    ));
}

#[test]
fn preserves_crlf_when_editing_before_line_break() -> Result<(), EditPlanError> {
    let edits = [edit(Position::new(0, 0), Position::new(0, 1), "A")];

    assert_eq!(
        apply_text_edits("a\r\nb", &edits, PositionEncoding::Utf8)?,
        "A\r\nb"
    );
    Ok(())
}

#[test]
fn rejects_utf8_byte_offset_inside_character() {
    let edits = [edit(Position::new(0, 1), Position::new(0, 2), "x")];

    assert!(matches!(
        apply_text_edits("😀", &edits, PositionEncoding::Utf8),
        Err(EditPlanError::InvalidPosition { .. })
    ));
}

#[test]
fn applies_utf32_edits_with_combining_text_and_eof() -> Result<(), EditPlanError> {
    let edits = [
        edit(Position::new(0, 1), Position::new(0, 2), "X"),
        edit(Position::new(1, 1), Position::new(1, 1), "!"),
    ];

    assert_eq!(
        apply_text_edits("e\u{301}😀\nZ", &edits, PositionEncoding::Utf32)?,
        "eX😀\nZ!"
    );
    Ok(())
}
