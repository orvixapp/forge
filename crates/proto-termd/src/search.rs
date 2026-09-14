//! Scrollback search over the plain-text dump of a session.
//!
//! The daemon formats the whole scrollable area (history plus active
//! screen) once per request, one line per grid row, and matches the query
//! here. That keeps Ghostty's render state and viewport untouched, so a
//! search never produces spurious dirty rows for attached clients.

use anyhow::{Context, Result};
use proto_ipc::SearchMatch;
use unicode_width::UnicodeWidthStr;

/// Upper bound on matches per request; a query like `.` on a 100k-line
/// scrollback must not produce a multi-megabyte reply.
pub const MAX_MATCHES: usize = 10_000;
const PREVIEW_CHARS: usize = 200;

/// Matches of `query` in `text`, where line `n` of `text` is absolute row
/// `n` of the scrollback. Columns count terminal cells, so wide characters
/// advance by two like they do on screen.
///
/// # Errors
///
/// An invalid regular expression, described for the user.
pub fn search_text(
    text: &str,
    query: &str,
    use_regex: bool,
    case_sensitive: bool,
) -> Result<Vec<SearchMatch>> {
    if query.is_empty() {
        return Ok(Vec::new());
    }
    let pattern = if use_regex {
        query.to_owned()
    } else {
        regex::escape(query)
    };
    let pattern = regex::RegexBuilder::new(&pattern)
        .case_insensitive(!case_sensitive)
        .size_limit(1 << 20)
        .build()
        .context("expresión regular inválida")?;
    let mut matches = Vec::new();
    for (row, line) in text.lines().enumerate() {
        for found in pattern.find_iter(line) {
            if found.is_empty() {
                continue;
            }
            let start = line[..found.start()].width();
            let end = start + found.as_str().width();
            matches.push(SearchMatch {
                row: row as u64,
                start: u16::try_from(start).unwrap_or(u16::MAX),
                end: u16::try_from(end).unwrap_or(u16::MAX),
                preview: preview(line),
            });
            if matches.len() >= MAX_MATCHES {
                return Ok(matches);
            }
        }
    }
    Ok(matches)
}

fn preview(line: &str) -> String {
    let trimmed = line.trim_end();
    if trimmed.chars().count() <= PREVIEW_CHARS {
        return trimmed.to_owned();
    }
    trimmed.chars().take(PREVIEW_CHARS - 1).collect::<String>() + "…"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_search_is_case_insensitive_by_default_and_reports_columns() {
        let text = "first line\nsecond Line\n\nlast\n";
        let found = search_text(text, "line", false, false).unwrap();
        assert_eq!(found.len(), 2);
        assert_eq!((found[0].row, found[0].start, found[0].end), (0, 6, 10));
        assert_eq!((found[1].row, found[1].start, found[1].end), (1, 7, 11));
        assert_eq!(found[1].preview, "second Line");
        assert_eq!(search_text(text, "line", false, true).unwrap().len(), 1);
        assert!(search_text(text, "", false, false).unwrap().is_empty());
    }

    #[test]
    fn literal_queries_are_not_regular_expressions() {
        let text = "a.c\nabc\n";
        assert_eq!(search_text(text, "a.c", false, false).unwrap().len(), 1);
        assert_eq!(search_text(text, "a.c", true, false).unwrap().len(), 2);
        assert!(search_text(text, "(", true, false).is_err());
    }

    #[test]
    fn wide_characters_take_two_columns_and_empty_matches_are_skipped() {
        let found = search_text("日本語 text", "text", false, false).unwrap();
        assert_eq!((found[0].start, found[0].end), (7, 11));
        assert!(search_text("abc", "x*", true, false).unwrap().is_empty());
    }

    #[test]
    fn results_are_capped() {
        let text = "x\n".repeat(MAX_MATCHES + 5);
        assert_eq!(
            search_text(&text, "x", false, false).unwrap().len(),
            MAX_MATCHES
        );
    }
}
