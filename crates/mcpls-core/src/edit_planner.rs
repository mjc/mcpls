//! Pure encoding-aware text edit planning.

use crate::bridge::{EncodingConverter, PositionEncoding};
use crate::workspace_edit::NormalizedTextEdit;

/// Errors raised while converting or ordering text edits.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum EditPlanError {
    /// A range refers to a line that does not exist.
    #[error("line {line} is outside the document")]
    InvalidLine {
        /// Zero-based line number that was requested.
        line: u32,
    },
    /// A character offset is invalid for the negotiated encoding.
    #[error("invalid character position on line {line}: {message}")]
    InvalidPosition {
        /// Zero-based line number containing the invalid position.
        line: u32,
        /// Converter detail explaining the invalid position.
        message: String,
    },
    /// The range end precedes the range start.
    #[error("edit range is reversed: {start}..{end}")]
    Reversed {
        /// Start byte offset.
        start: usize,
        /// End byte offset.
        end: usize,
    },
    /// Two edits overlap or have ambiguous identical insertion points.
    #[error("edits overlap at byte range {start}..{end}")]
    Overlapping {
        /// Start byte offset of the later edit.
        start: usize,
        /// End byte offset of the later edit.
        end: usize,
    },
}

/// Convert and apply non-overlapping text edits without filesystem I/O.
///
/// # Errors
///
/// Returns an error when a range is outside the document, uses an invalid
/// character offset, is reversed, or overlaps another edit.
pub fn apply_text_edits(
    content: &str,
    edits: &[NormalizedTextEdit],
    encoding: PositionEncoding,
) -> Result<String, EditPlanError> {
    let converter = EncodingConverter::new(encoding);
    let mut spans = Vec::with_capacity(edits.len());
    for edit in edits {
        let start = position_to_byte(content, edit.range.start, &converter)?;
        let end = position_to_byte(content, edit.range.end, &converter)?;
        if start > end {
            return Err(EditPlanError::Reversed { start, end });
        }
        spans.push((start, end, edit.new_text.as_str()));
    }

    spans.sort_by_key(|(start, end, _)| (*start, *end));
    for pair in spans.windows(2) {
        let (_, previous_end, _) = pair[0];
        let (start, end, _) = pair[1];
        if start < previous_end || start == pair[0].0 {
            return Err(EditPlanError::Overlapping { start, end });
        }
    }

    let mut result = content.to_string();
    for (start, end, replacement) in spans.into_iter().rev() {
        result.replace_range(start..end, replacement);
    }
    Ok(result)
}

fn position_to_byte(
    content: &str,
    position: lsp_types::Position,
    converter: &EncodingConverter,
) -> Result<usize, EditPlanError> {
    let line_start = line_start(content, position.line)?;
    let mut line_end = content[line_start..]
        .find('\n')
        .map_or(content.len(), |offset| line_start + offset);
    if line_end > line_start && content.as_bytes()[line_end - 1] == b'\r' {
        line_end -= 1;
    }
    let line = &content[line_start..line_end];
    converter
        .character_to_byte_offset(line, position.character)
        .map(|offset| line_start + offset)
        .map_err(|message| EditPlanError::InvalidPosition {
            line: position.line,
            message,
        })
}

fn line_start(content: &str, line: u32) -> Result<usize, EditPlanError> {
    if line == 0 {
        return Ok(0);
    }
    let mut current = 0;
    for (index, byte) in content.bytes().enumerate() {
        if byte == b'\n' {
            current += 1;
            if current == line {
                return Ok(index + 1);
            }
        }
    }
    Err(EditPlanError::InvalidLine { line })
}

#[cfg(test)]
mod tests {
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
}
