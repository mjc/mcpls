//! Bounded lexical matching primitives for project-scoped text search.

use std::ops::Range;

use regex::RegexBuilder;

/// Matching interpretation for a lexical query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LexicalMatchMode {
    /// Treat the query as literal text.
    Literal,
    /// Interpret the query with Rust regex syntax.
    Regex,
}

/// Case behavior for a lexical query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LexicalCaseMode {
    /// Match exact case.
    Sensitive,
    /// Ignore case.
    Insensitive,
    /// Ignore case unless the query contains an uppercase character.
    Smart,
}

/// Find every non-overlapping match using the selected lexical semantics.
///
/// Regex mode uses the Rust `regex` dialect; multiline enables line anchors
/// without enabling dot-all behavior.
pub(crate) fn find_matches(
    text: &str,
    query: &str,
    mode: LexicalMatchMode,
    case: LexicalCaseMode,
    multiline: bool,
) -> Result<Vec<Range<usize>>, String> {
    if query.is_empty() {
        return Err("lexical query must not be empty".to_owned());
    }
    let pattern = match mode {
        LexicalMatchMode::Literal => regex::escape(query),
        LexicalMatchMode::Regex => query.to_owned(),
    };
    let case_insensitive = match case {
        LexicalCaseMode::Sensitive => false,
        LexicalCaseMode::Insensitive => true,
        LexicalCaseMode::Smart => !query.chars().any(char::is_uppercase),
    };
    let regex = RegexBuilder::new(&pattern)
        .case_insensitive(case_insensitive)
        .multi_line(multiline)
        .build()
        .map_err(|error| format!("invalid lexical regex: {error}"))?;
    Ok(regex
        .find_iter(text)
        .map(|matched| matched.start()..matched.end())
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_metacharacters_are_not_regex() {
        let matches = find_matches(
            "a.b\naXb",
            "a.b",
            LexicalMatchMode::Literal,
            LexicalCaseMode::Sensitive,
            false,
        )
        .unwrap();

        assert_eq!(matches, vec![0..3]);
    }

    #[test]
    fn regex_and_smart_case_follow_the_selected_mode() {
        let regex_matches = find_matches(
            "item-12 item-abc",
            r"item-\d+",
            LexicalMatchMode::Regex,
            LexicalCaseMode::Sensitive,
            false,
        )
        .unwrap();
        assert_eq!(regex_matches, vec![0..7]);

        let insensitive = find_matches(
            "Needle needle",
            "needle",
            LexicalMatchMode::Literal,
            LexicalCaseMode::Smart,
            false,
        )
        .unwrap();
        assert_eq!(insensitive, vec![0..6, 7..13]);

        let sensitive = find_matches(
            "Needle needle",
            "Needle",
            LexicalMatchMode::Literal,
            LexicalCaseMode::Smart,
            false,
        )
        .unwrap();
        assert_eq!(sensitive, vec![0..6]);

        let explicit = find_matches(
            "Needle needle",
            "Needle",
            LexicalMatchMode::Literal,
            LexicalCaseMode::Insensitive,
            false,
        )
        .unwrap();
        assert_eq!(explicit, vec![0..6, 7..13]);
    }
}
