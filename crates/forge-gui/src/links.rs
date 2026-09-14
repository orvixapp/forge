//! Clickable things in terminal text: URLs and `path:line:col` references
//! found around a column, plus the paste-safety rule.
//!
//! Detection runs only on the row under a Ctrl+click or Ctrl+hover, never
//! per frame, so it can afford to be simple and allocation-happy.

use std::ops::Range;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkTarget {
    Url(String),
    /// A file path as written (possibly relative), with an optional
    /// `:line` / `:line:col` suffix.
    File {
        path: String,
        line: Option<u32>,
        column: Option<u32>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    /// Columns the link occupies in the row.
    pub range: Range<usize>,
    pub target: LinkTarget,
}

/// Characters that end a token but often trail a URL or path in prose.
const TRAILING_PUNCTUATION: &[char] = &['.', ',', ';', ':', ')', ']', '}', '>', '\'', '"', '`'];
const OPENING: &[char] = &['(', '[', '{', '<', '\'', '"', '`'];

/// The link under column `col` of `text`, where `text` has exactly one
/// character per column (see `TerminalGrid::row_text_padded`).
#[must_use]
pub fn link_at(text: &str, col: usize) -> Option<Link> {
    let chars: Vec<char> = text.chars().collect();
    if col >= chars.len() || chars[col].is_whitespace() {
        return None;
    }
    let mut start = col;
    while start > 0 && !chars[start - 1].is_whitespace() {
        start -= 1;
    }
    let mut end = col + 1;
    while end < chars.len() && !chars[end].is_whitespace() {
        end += 1;
    }
    // Strip wrapping brackets/quotes and trailing prose punctuation.
    while start < end && OPENING.contains(&chars[start]) {
        start += 1;
    }
    while end > start && TRAILING_PUNCTUATION.contains(&chars[end - 1]) {
        end -= 1;
    }
    if !(start..end).contains(&col) {
        return None;
    }
    let token: String = chars[start..end].iter().collect();
    let target = classify(&token)?;
    Some(Link {
        range: start..end,
        target,
    })
}

fn classify(token: &str) -> Option<LinkTarget> {
    if let Some((scheme, rest)) = token.split_once("://")
        && !scheme.is_empty()
        && scheme.chars().all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '.')
        && !rest.is_empty()
    {
        return Some(LinkTarget::Url(token.to_owned()));
    }
    if let Some(rest) = token.strip_prefix("mailto:")
        && rest.contains('@')
    {
        return Some(LinkTarget::Url(token.to_owned()));
    }
    if token.starts_with("www.") && token.len() > 4 {
        return Some(LinkTarget::Url(format!("https://{token}")));
    }
    file_reference(token)
}

/// `path`, `path:line` or `path:line:col`, where `path` looks like a file
/// system path: contains a separator, starts with `~`/`.`, or has a
/// recognisable extension.
fn file_reference(token: &str) -> Option<LinkTarget> {
    let mut parts = token.rsplitn(3, ':');
    let mut path = token;
    let mut line = None;
    let mut column = None;
    // Trailing numeric segments are line/column; a non-numeric one keeps
    // the colon inside the path (`C:` is not a line number on Unix rows).
    if let Some(last) = parts.next()
        && let Some(middle) = parts.next()
    {
        if let (Ok(l), Ok(c)) = (middle.parse::<u32>(), last.parse::<u32>())
            && let Some(rest) = parts.next()
        {
            path = rest;
            line = Some(l);
            column = Some(c);
        } else if let Ok(l) = last.parse::<u32>() {
            path = &token[..token.len() - last.len() - 1];
            line = Some(l);
        }
    }
    let path = path.trim_end_matches(':');
    if path.is_empty() || path.starts_with("://") {
        return None;
    }
    let looks_like_path = path.contains('/')
        || path.starts_with('~')
        || path.starts_with('.')
        || path
            .rsplit_once('.')
            .is_some_and(|(stem, ext)| !stem.is_empty() && ext.chars().all(char::is_alphanumeric) && (1..=5).contains(&ext.len()));
    looks_like_path.then(|| LinkTarget::File {
        path: path.to_owned(),
        line,
        column,
    })
}

/// Why a paste deserves a confirmation, if it does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasteRisk {
    /// Newlines outside bracketed paste run each line as a command.
    Multiline,
    /// Contains the bracketed-paste end sequence, which lets pasted text
    /// escape the bracket and inject commands.
    BracketEscape,
}

/// Ghostty's `paste_is_safe` rule, made terminal-state aware: newlines are
/// fine when the application enabled bracketed paste.
#[must_use]
pub fn paste_risk(text: &str, bracketed_paste: bool) -> Option<PasteRisk> {
    if text.contains("\x1b[201~") {
        return Some(PasteRisk::BracketEscape);
    }
    if !bracketed_paste && text.contains(['\n', '\r']) {
        return Some(PasteRisk::Multiline);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_are_found_under_the_cursor_without_trailing_punctuation() {
        let text = "see (https://forge.dev/docs), thanks";
        let link = link_at(text, 10).unwrap();
        assert_eq!(link.range, 5..27);
        assert_eq!(
            link.target,
            LinkTarget::Url("https://forge.dev/docs".into())
        );
        assert_eq!(link_at(text, 3), None, "plain words are not links");
        assert_eq!(link_at(text, 4), None, "the bracket itself is not");
        assert_eq!(
            link_at("go to www.example.org now", 8).unwrap().target,
            LinkTarget::Url("https://www.example.org".into())
        );
        assert_eq!(link_at("   ", 1), None);
        assert_eq!(link_at("x", 5), None);
    }

    #[test]
    fn file_references_carry_line_and_column() {
        let link = link_at("error: src/main.rs:42:7: oops", 12).unwrap();
        assert_eq!(link.range, 7..23);
        assert_eq!(
            link.target,
            LinkTarget::File {
                path: "src/main.rs".into(),
                line: Some(42),
                column: Some(7),
            }
        );
        assert_eq!(
            link_at("at ./notes.md:3", 5).unwrap().target,
            LinkTarget::File {
                path: "./notes.md".into(),
                line: Some(3),
                column: None,
            }
        );
        assert_eq!(
            link_at("~/.config/forge/config.toml", 2).unwrap().target,
            LinkTarget::File {
                path: "~/.config/forge/config.toml".into(),
                line: None,
                column: None,
            }
        );
        assert_eq!(link_at("Cargo.toml", 0).unwrap().range, 0..10);
        assert_eq!(link_at("12:30", 1), None, "a clock is not a file");
        assert_eq!(link_at("hello", 1), None);
    }

    #[test]
    fn paste_risk_follows_the_bracketed_paste_mode() {
        assert_eq!(paste_risk("ls\n", false), Some(PasteRisk::Multiline));
        assert_eq!(paste_risk("ls\n", true), None);
        assert_eq!(paste_risk("ls", false), None);
        assert_eq!(
            paste_risk("x\x1b[201~rm -rf\n", true),
            Some(PasteRisk::BracketEscape)
        );
    }
}
