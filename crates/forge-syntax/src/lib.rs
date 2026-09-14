//! tree-sitter behind the editor (ARCHITECTURE.md §13.3): a registry of
//! languages with their highlight queries, incremental parsing over rope
//! snapshots, and per-line highlight spans mapped to theme tokens.
//!
//! Grammars are compiled in for now (`tree-sitter-*` crates); loading
//! `.so` grammars on demand is the follow-up once a grammar directory
//! exists in the config.

use ropey::Rope;
use std::{ops::Range, path::Path, time::Instant};
use streaming_iterator::StreamingIterator;
use tree_sitter::{InputEdit, Language, Parser, Point, Query, QueryCursor, Tree};

/// Theme slot a capture maps to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Token {
    Keyword,
    String,
    Comment,
    Function,
    Type,
    Variable,
    Number,
    Constant,
    Operator,
    Punctuation,
    Attribute,
    Property,
    /// Markup headings, tags, labels.
    Tag,
}

impl Token {
    /// Capture name (`keyword`, `function.method`, …) to token; the first
    /// segment decides, so grammar-specific suffixes need no table.
    #[must_use]
    pub fn from_capture(name: &str) -> Option<Self> {
        let head = name.split('.').next().unwrap_or(name);
        Some(match head {
            "keyword" | "include" | "repeat" | "conditional" | "storageclass" => Self::Keyword,
            "string" | "character" | "escape" => Self::String,
            "comment" => Self::Comment,
            "function" | "method" | "constructor" => Self::Function,
            "type" | "namespace" | "module" => Self::Type,
            "variable" | "parameter" | "field" => Self::Variable,
            "number" | "float" | "boolean" => Self::Number,
            "constant" => Self::Constant,
            "operator" => Self::Operator,
            "punctuation" | "delimiter" => Self::Punctuation,
            "attribute" | "preproc" | "decorator" => Self::Attribute,
            "property" => Self::Property,
            "tag" | "label" | "markup" | "text" | "title" | "embedded" => Self::Tag,
            _ => return None,
        })
    }
}

/// A bundled grammar and how to recognise its files.
pub struct LanguageConfig {
    pub name: &'static str,
    pub extensions: &'static [&'static str],
    /// Interpreter names accepted in a `#!` line.
    pub shebangs: &'static [&'static str],
    language: fn() -> Language,
    highlights: &'static str,
}

pub const LANGUAGES: &[LanguageConfig] = &[
    LanguageConfig {
        name: "rust",
        extensions: &["rs"],
        shebangs: &[],
        language: || tree_sitter_rust::LANGUAGE.into(),
        highlights: tree_sitter_rust::HIGHLIGHTS_QUERY,
    },
    LanguageConfig {
        name: "toml",
        extensions: &["toml"],
        shebangs: &[],
        language: || tree_sitter_toml_ng::LANGUAGE.into(),
        highlights: tree_sitter_toml_ng::HIGHLIGHTS_QUERY,
    },
    LanguageConfig {
        name: "json",
        extensions: &["json", "jsonc"],
        shebangs: &[],
        language: || tree_sitter_json::LANGUAGE.into(),
        highlights: tree_sitter_json::HIGHLIGHTS_QUERY,
    },
    LanguageConfig {
        name: "bash",
        extensions: &["sh", "bash", "zsh"],
        shebangs: &["sh", "bash", "zsh", "dash"],
        language: || tree_sitter_bash::LANGUAGE.into(),
        highlights: tree_sitter_bash::HIGHLIGHT_QUERY,
    },
    LanguageConfig {
        name: "python",
        extensions: &["py", "pyi"],
        shebangs: &["python", "python3"],
        language: || tree_sitter_python::LANGUAGE.into(),
        highlights: tree_sitter_python::HIGHLIGHTS_QUERY,
    },
    LanguageConfig {
        name: "javascript",
        extensions: &["js", "mjs", "cjs", "jsx"],
        shebangs: &["node"],
        language: || tree_sitter_javascript::LANGUAGE.into(),
        highlights: tree_sitter_javascript::HIGHLIGHT_QUERY,
    },
    LanguageConfig {
        name: "markdown",
        extensions: &["md", "markdown"],
        shebangs: &[],
        language: || tree_sitter_md::LANGUAGE.into(),
        highlights: tree_sitter_md::HIGHLIGHT_QUERY_BLOCK,
    },
    LanguageConfig {
        name: "c",
        extensions: &["c", "h"],
        shebangs: &[],
        language: || tree_sitter_c::LANGUAGE.into(),
        highlights: tree_sitter_c::HIGHLIGHT_QUERY,
    },
];

/// Language for a file, by extension then by shebang.
#[must_use]
pub fn detect(path: Option<&Path>, first_line: &str) -> Option<&'static LanguageConfig> {
    if let Some(extension) = path
        .and_then(Path::extension)
        .and_then(|extension| extension.to_str())
    {
        let extension = extension.to_ascii_lowercase();
        if let Some(language) = LANGUAGES
            .iter()
            .find(|language| language.extensions.contains(&extension.as_str()))
        {
            return Some(language);
        }
    }
    let shebang = first_line.strip_prefix("#!")?;
    let interpreter = shebang
        .split_whitespace()
        .filter_map(|part| part.rsplit('/').next())
        .find(|part| *part != "env")?;
    LANGUAGES
        .iter()
        .find(|language| language.shebangs.contains(&interpreter))
}

/// A highlighted run inside one line: byte range within the line's text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    pub range: Range<usize>,
    pub token: Token,
}

/// Parser, tree and query for one buffer.
pub struct SyntaxState {
    config: &'static LanguageConfig,
    parser: Parser,
    query: Query,
    tokens: Vec<Option<Token>>,
    tree: Option<Tree>,
    /// Version of the buffer the tree corresponds to.
    pub version: u64,
}

/// Parsing budget per call on the UI thread; longer parses keep the old
/// tree and retry on the next tick.
pub const PARSE_BUDGET_MS: u128 = 8;

impl SyntaxState {
    /// Prepares a parser for `config`; the query is compiled once.
    ///
    /// # Errors
    ///
    /// Grammar/binding version mismatch or an invalid bundled query.
    pub fn new(config: &'static LanguageConfig) -> Result<Self, String> {
        let language = (config.language)();
        let mut parser = Parser::new();
        parser
            .set_language(&language)
            .map_err(|error| format!("{}: {error}", config.name))?;
        let query = Query::new(&language, config.highlights)
            .map_err(|error| format!("{} highlights: {error}", config.name))?;
        let tokens = query
            .capture_names()
            .iter()
            .map(|name| Token::from_capture(name))
            .collect();
        Ok(Self {
            config,
            parser,
            query,
            tokens,
            tree: None,
            version: 0,
        })
    }

    #[must_use]
    pub fn language_name(&self) -> &'static str {
        self.config.name
    }

    #[must_use]
    pub fn has_tree(&self) -> bool {
        self.tree.is_some()
    }

    /// Tells the tree about an edit so the next parse is incremental.
    pub fn edited(&mut self, edit: &InputEdit) {
        if let Some(tree) = &mut self.tree {
            tree.edit(edit);
        }
    }

    /// Forgets the tree; the next parse starts from scratch.
    pub fn invalidate(&mut self) {
        self.tree = None;
    }

    /// Parses `text` (a snapshot at `version`), incrementally when a tree
    /// exists. Returns `false` when the budget ran out; the old tree stays.
    pub fn parse(&mut self, text: &Rope, version: u64, budget_ms: Option<u128>) -> bool {
        let started = Instant::now();
        let mut over_budget = |_: &tree_sitter::ParseState| {
            if budget_ms.is_some_and(|budget| started.elapsed().as_millis() > budget) {
                std::ops::ControlFlow::Break(())
            } else {
                std::ops::ControlFlow::Continue(())
            }
        };
        let options = tree_sitter::ParseOptions::new().progress_callback(&mut over_budget);
        let mut read = |byte: usize, _: Point| -> &[u8] {
            if byte >= text.len_bytes() {
                return &[];
            }
            let (chunk, chunk_start, _, _) = text.chunk_at_byte(byte);
            &chunk.as_bytes()[byte - chunk_start..]
        };
        match self
            .parser
            .parse_with_options(&mut read, self.tree.as_ref(), Some(options))
        {
            Some(tree) => {
                self.tree = Some(tree);
                self.version = version;
                true
            }
            None => false,
        }
    }

    /// Highlight spans of the line covering bytes `line_range` of `text`,
    /// with byte ranges relative to the line start. Inner captures win
    /// over enclosing ones, like tree-sitter-highlight.
    #[must_use]
    pub fn line_spans(&self, text: &Rope, line_range: Range<usize>) -> Vec<Span> {
        let Some(tree) = &self.tree else {
            return Vec::new();
        };
        let mut cursor = QueryCursor::new();
        cursor.set_byte_range(line_range.clone());
        let provider = |node: tree_sitter::Node| {
            text.byte_slice(node.byte_range())
                .chunks()
                .map(str::as_bytes)
        };
        let mut captures = cursor.captures(&self.query, tree.root_node(), provider);
        let mut raw: Vec<(Range<usize>, Token)> = Vec::new();
        while let Some((matched, index)) = captures.next() {
            let capture = matched.captures()[*index];
            let Some(token) = self.tokens.get(capture.index as usize).copied().flatten() else {
                continue;
            };
            let range = capture.node.byte_range();
            let start = range.start.max(line_range.start);
            let end = range.end.min(line_range.end);
            if start < end {
                raw.push((start - line_range.start..end - line_range.start, token));
            }
        }
        resolve_overlaps(raw)
    }
}

/// Flattens possibly-nested capture ranges into disjoint spans where the
/// innermost (shortest) capture wins.
fn resolve_overlaps(mut raw: Vec<(Range<usize>, Token)>) -> Vec<Span> {
    if raw.is_empty() {
        return Vec::new();
    }
    // Outer captures first, so inner ones painted later override them.
    raw.sort_by_key(|(range, _)| (range.start, std::cmp::Reverse(range.len())));
    let end = raw.iter().map(|(range, _)| range.end).max().unwrap_or(0);
    let mut cells: Vec<Option<Token>> = vec![None; end];
    for (range, token) in raw {
        for cell in &mut cells[range] {
            *cell = Some(token);
        }
    }
    let mut spans: Vec<Span> = Vec::new();
    for (index, cell) in cells.into_iter().enumerate() {
        let Some(token) = cell else { continue };
        match spans.last_mut() {
            Some(last) if last.token == token && last.range.end == index => last.range.end += 1,
            _ => spans.push(Span {
                range: index..index + 1,
                token,
            }),
        }
    }
    spans
}

/// Builds tree-sitter's edit description from a change already applied to
/// `text`: the new start char, the inserted text and the removed text.
#[must_use]
pub fn input_edit(text: &Rope, new_start_char: usize, inserted: &str, removed: &str) -> InputEdit {
    let start_byte = text.char_to_byte(new_start_char.min(text.len_chars()));
    let start_line = text.byte_to_line(start_byte);
    let start_position = Point {
        row: start_line,
        column: start_byte - text.line_to_byte(start_line),
    };
    let advance = |from: Point, inserted: &str| -> Point {
        match inserted.rsplit_once('\n') {
            Some((head, tail)) => Point {
                row: from.row + head.matches('\n').count() + 1,
                column: tail.len(),
            },
            None => Point {
                row: from.row,
                column: from.column + inserted.len(),
            },
        }
    };
    InputEdit {
        start_byte,
        old_end_byte: start_byte + removed.len(),
        new_end_byte: start_byte + inserted.len(),
        start_position,
        old_end_position: advance(start_position, removed),
        new_end_position: advance(start_position, inserted),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_languages_by_extension_and_shebang() {
        assert_eq!(
            detect(Some(Path::new("a/b.RS")), "").map(|l| l.name),
            Some("rust")
        );
        assert_eq!(
            detect(Some(Path::new("run")), "#!/usr/bin/env python3\n").map(|l| l.name),
            Some("python")
        );
        assert_eq!(
            detect(Some(Path::new("x.unknown")), "plain").map(|l| l.name),
            None
        );
        assert_eq!(detect(None, "#!/bin/sh").map(|l| l.name), Some("bash"));
    }

    #[test]
    fn every_bundled_grammar_compiles_its_query() {
        for config in LANGUAGES {
            let state = SyntaxState::new(config).unwrap_or_else(|error| panic!("{error}"));
            assert!(
                state.tokens.iter().any(Option::is_some),
                "{} maps no captures",
                config.name
            );
        }
    }

    #[test]
    fn rust_highlights_keywords_strings_and_comments_per_line() {
        let config = LANGUAGES.iter().find(|l| l.name == "rust").unwrap();
        let mut state = SyntaxState::new(config).unwrap();
        let text = Rope::from_str("fn main() {\n    let s = \"hi\"; // note\n}\n");
        assert!(state.parse(&text, 1, None));
        let line1 = text.line_to_byte(1)..text.line_to_byte(2);
        let spans = state.line_spans(&text, line1);
        let token_at = |offset: usize| {
            spans
                .iter()
                .find(|span| span.range.contains(&offset))
                .map(|span| span.token)
        };
        assert_eq!(token_at(4), Some(Token::Keyword), "let");
        assert_eq!(token_at(13), Some(Token::String), "\"hi\"");
        assert_eq!(token_at(20), Some(Token::Comment), "// note");
        let line0 = 0..text.line_to_byte(1);
        let spans = state.line_spans(&text, line0);
        assert_eq!(spans[0].token, Token::Keyword);
        assert!(spans.iter().any(|span| span.token == Token::Function));
        for pair in spans.windows(2) {
            assert!(
                pair[0].range.end <= pair[1].range.start,
                "spans overlap: {spans:?}"
            );
        }
    }

    #[test]
    fn incremental_parse_follows_edits() {
        let config = LANGUAGES.iter().find(|l| l.name == "rust").unwrap();
        let mut state = SyntaxState::new(config).unwrap();
        let mut text = Rope::from_str("fn a() {}\n");
        assert!(state.parse(&text, 1, None));
        // Insert a second function after the first line.
        let inserted = "fn b() {}\n";
        text.insert(text.line_to_char(1), inserted);
        let edit = input_edit(&text, text.line_to_char(1), inserted, "");
        assert_eq!(edit.start_byte, 10);
        assert_eq!(edit.new_end_position, Point { row: 2, column: 0 });
        state.edited(&edit);
        assert!(state.parse(&text, 2, Some(PARSE_BUDGET_MS)));
        let line1 = text.line_to_byte(1)..text.line_to_byte(2);
        let spans = state.line_spans(&text, line1);
        assert_eq!(spans.first().map(|span| span.token), Some(Token::Keyword));
        assert_eq!(state.version, 2);
    }

    #[test]
    fn input_edit_counts_removed_and_inserted_lines() {
        let text = Rope::from_str("ab\ncd\n");
        let edit = input_edit(&text, 3, "cd", "x\ny\nz");
        assert_eq!(edit.start_position, Point { row: 1, column: 0 });
        assert_eq!(edit.old_end_position, Point { row: 3, column: 1 });
        assert_eq!(edit.new_end_position, Point { row: 1, column: 2 });
        assert_eq!(edit.old_end_byte, 8);
        assert_eq!(edit.new_end_byte, 5);
    }
}
