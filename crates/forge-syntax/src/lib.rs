//! tree-sitter behind the editor (ARCHITECTURE.md §13.3): a registry of
//! languages with their highlight queries, incremental parsing over rope
//! snapshots, and per-line highlight spans mapped to theme tokens.
//!
//! Grammars are compiled in for now (`tree-sitter-*` crates); loading
//! `.so` grammars on demand is the follow-up once a grammar directory
//! exists in the config.

use ropey::Rope;
use std::{
    ops::Range,
    path::Path,
    sync::{Arc, mpsc},
};
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

/// tree-sitter state for one buffer. Parsing runs on a worker thread that
/// owns the `Parser`; the UI keeps the last tree it received, applies
/// edits to it ahead of time (so highlights follow the text while the
/// worker catches up) and adopts fresh trees as they arrive.
pub struct SyntaxState {
    config: &'static LanguageConfig,
    query: Arc<Query>,
    tokens: Arc<Vec<Option<Token>>>,
    tree: Option<Tree>,
    /// Buffer version the UI tree describes (edits applied ahead count).
    pub version: u64,
    /// Version of the last tree the worker produced.
    pub parsed_version: u64,
    requests: mpsc::Sender<Request>,
    replies: mpsc::Receiver<(u64, Tree)>,
    /// Edits applied to the UI tree since the last adopted parse, replayed
    /// onto trees that arrive for older versions.
    pending: Vec<(u64, InputEdit)>,
    /// Spans of the last requested line range, valid for `generation`.
    cache: Option<HighlightCache>,
    generation: u64,
}

enum Request {
    Edit(InputEdit),
    Parse(Rope, u64),
    Reset,
}

struct HighlightCache {
    generation: u64,
    lines: Range<usize>,
    spans: Vec<Vec<Span>>,
}

/// Longest an incremental parse of one keystroke may take on the UI
/// thread; kept for API compatibility with callers that pass a budget to
/// [`SyntaxState::parse`]. Parsing itself now happens off-thread.
pub const PARSE_BUDGET_MS: u128 = 8;

impl SyntaxState {
    /// Prepares a parser for `config` on a worker thread; the query is
    /// compiled once.
    ///
    /// # Errors
    ///
    /// Grammar/binding version mismatch, an invalid bundled query or a
    /// thread that cannot be spawned.
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
        let (requests, worker_rx) = mpsc::channel();
        let (worker_tx, replies) = mpsc::channel();
        std::thread::Builder::new()
            .name(format!("forge-syntax-{}", config.name))
            .spawn(move || worker(parser, &worker_rx, &worker_tx))
            .map_err(|error| error.to_string())?;
        Ok(Self {
            config,
            query: Arc::new(query),
            tokens: Arc::new(tokens),
            tree: None,
            version: 0,
            parsed_version: 0,
            requests,
            replies,
            pending: Vec::new(),
            cache: None,
            generation: 0,
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

    /// Records an edit that took the text to `version`: the UI tree shifts
    /// immediately and the worker applies it before its next parse.
    pub fn edited(&mut self, edit: &InputEdit, version: u64) {
        if let Some(tree) = &mut self.tree {
            tree.edit(edit);
        }
        self.version = version;
        self.pending.push((version, *edit));
        self.cache = None;
        let _ = self.requests.send(Request::Edit(*edit));
    }

    /// Forgets the tree; the next parse starts from scratch.
    pub fn invalidate(&mut self) {
        self.tree = None;
        self.cache = None;
        self.pending.clear();
        let _ = self.requests.send(Request::Reset);
    }

    /// Asks the worker for a tree of `text` (a snapshot at `version`).
    /// Returns immediately; [`Self::poll`] adopts the result. The budget
    /// is ignored: parsing never blocks the caller.
    pub fn parse(&mut self, text: &Rope, version: u64, _budget_ms: Option<u128>) -> bool {
        self.version = version;
        self.requests
            .send(Request::Parse(text.clone(), version))
            .is_ok()
    }

    /// Adopts trees the worker finished. Returns whether highlights changed.
    pub fn poll(&mut self) -> bool {
        let mut adopted = false;
        while let Ok((version, mut tree)) = self.replies.try_recv() {
            if version < self.parsed_version {
                continue;
            }
            // Bring an older tree up to the UI's text with the edits it
            // has not seen; the worker will deliver the real thing later.
            for (edit_version, edit) in &self.pending {
                if *edit_version > version {
                    tree.edit(edit);
                }
            }
            self.pending
                .retain(|(edit_version, _)| *edit_version > version);
            self.parsed_version = version;
            self.tree = Some(tree);
            adopted = true;
        }
        if adopted {
            self.cache = None;
            self.generation += 1;
        }
        adopted
    }

    /// Whether the worker still owes a tree for the current text.
    #[must_use]
    pub fn is_stale(&self) -> bool {
        self.parsed_version < self.version || self.tree.is_none()
    }

    /// Highlight spans for each line in `lines` (byte ranges relative to
    /// each line's start). One query run covers the whole range and the
    /// result is cached until the tree or the text changes.
    pub fn highlights(&mut self, text: &Rope, lines: Range<usize>) -> Vec<Vec<Span>> {
        if let Some(cache) = &self.cache
            && cache.generation == self.generation
            && cache.lines.start <= lines.start
            && cache.lines.end >= lines.end
        {
            let offset = lines.start - cache.lines.start;
            return cache.spans[offset..offset + lines.len()].to_vec();
        }
        let lines = lines.start..lines.end.min(text.len_lines());
        let spans = self.range_highlights(text, lines.clone());
        self.cache = Some(HighlightCache {
            generation: self.generation,
            lines,
            spans: spans.clone(),
        });
        spans
    }

    fn range_highlights(&self, text: &Rope, lines: Range<usize>) -> Vec<Vec<Span>> {
        let mut per_line: Vec<Vec<(Range<usize>, Token)>> = vec![Vec::new(); lines.len()];
        let Some(tree) = &self.tree else {
            return per_line.into_iter().map(|_| Vec::new()).collect();
        };
        if lines.is_empty() {
            return Vec::new();
        }
        let starts: Vec<usize> = (lines.start..=lines.end)
            .map(|line| {
                if line < text.len_lines() {
                    text.line_to_byte(line)
                } else {
                    text.len_bytes()
                }
            })
            .collect();
        let byte_range = starts[0]..starts[starts.len() - 1];
        let mut cursor = QueryCursor::new();
        cursor.set_byte_range(byte_range.clone());
        let provider = |node: tree_sitter::Node| {
            text.byte_slice(
                node.byte_range().start.min(text.len_bytes())
                    ..node.byte_range().end.min(text.len_bytes()),
            )
            .chunks()
            .map(str::as_bytes)
        };
        let mut captures = cursor.captures(&self.query, tree.root_node(), provider);
        while let Some((matched, index)) = captures.next() {
            let capture = matched.captures()[*index];
            let Some(token) = self.tokens.get(capture.index as usize).copied().flatten() else {
                continue;
            };
            let range = capture.node.byte_range();
            let start = range.start.max(byte_range.start);
            let end = range.end.min(byte_range.end);
            if start >= end {
                continue;
            }
            // A capture may span several lines: cut it at line starts.
            let first = starts.partition_point(|line_start| *line_start <= start) - 1;
            for (offset, line_start) in starts[first..starts.len() - 1].iter().enumerate() {
                let line_end = starts[first + offset + 1];
                if *line_start >= end {
                    break;
                }
                let from = start.max(*line_start) - line_start;
                let to = end.min(line_end) - line_start;
                if from < to {
                    per_line[first + offset].push((from..to, token));
                }
            }
        }
        per_line.into_iter().map(resolve_overlaps).collect()
    }

    /// Spans of one line, for tests and one-off callers; uncached.
    #[must_use]
    pub fn line_spans(&self, text: &Rope, line_range: Range<usize>) -> Vec<Span> {
        let line = text.byte_to_line(line_range.start.min(text.len_bytes()));
        self.range_highlights(text, line..line + 1)
            .into_iter()
            .next()
            .unwrap_or_default()
    }
}

/// The parsing thread: keeps its own tree, applies edits in order and
/// parses the newest snapshot, coalescing requests that piled up.
fn worker(
    mut parser: Parser,
    requests: &mpsc::Receiver<Request>,
    replies: &mpsc::Sender<(u64, Tree)>,
) {
    let mut tree: Option<Tree> = None;
    while let Ok(first) = requests.recv() {
        let mut snapshot: Option<(Rope, u64)> = None;
        let mut handle = |request: Request| match request {
            Request::Edit(edit) => {
                if let Some(tree) = &mut tree {
                    tree.edit(&edit);
                }
            }
            Request::Parse(rope, version) => snapshot = Some((rope, version)),
            Request::Reset => tree = None,
        };
        handle(first);
        while let Ok(request) = requests.try_recv() {
            handle(request);
        }
        let Some((rope, version)) = snapshot else {
            continue;
        };
        let mut read = |byte: usize, _: Point| -> &[u8] {
            if byte >= rope.len_bytes() {
                return &[];
            }
            let (chunk, chunk_start, _, _) = rope.chunk_at_byte(byte);
            &chunk.as_bytes()[byte - chunk_start..]
        };
        if let Some(parsed) = parser.parse_with_options(&mut read, tree.as_ref(), None) {
            tree = Some(parsed.clone());
            if replies.send((version, parsed)).is_err() {
                return;
            }
        }
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
    use std::time::{Duration, Instant};

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

    /// Requests a parse and blocks until the worker delivered it.
    fn parse_now(state: &mut SyntaxState, text: &Rope, version: u64) {
        assert!(state.parse(text, version, None));
        let deadline = Instant::now() + Duration::from_secs(10);
        while state.is_stale() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
            state.poll();
        }
        assert!(
            !state.is_stale(),
            "parse of version {version} never arrived"
        );
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
        parse_now(&mut state, &text, 1);
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
    fn range_highlights_match_per_line_queries_and_cache() {
        let config = LANGUAGES.iter().find(|l| l.name == "rust").unwrap();
        let mut state = SyntaxState::new(config).unwrap();
        let text = Rope::from_str(
            "fn main() {\n    let s = \"hi\"; // note\n    /* multi\n    line */ x();\n}\n",
        );
        parse_now(&mut state, &text, 1);
        let all = state.highlights(&text, 0..5);
        assert_eq!(all.len(), 5);
        for (line, spans) in all.iter().enumerate() {
            let start = text.line_to_byte(line);
            let end = if line + 1 < text.len_lines() {
                text.line_to_byte(line + 1)
            } else {
                text.len_bytes()
            };
            assert_eq!(spans, &state.line_spans(&text, start..end), "line {line}");
        }
        assert!(
            all[3].iter().any(|span| span.token == Token::Comment),
            "comment continues on line 3"
        );
        let sub = state.highlights(&text, 1..3);
        assert_eq!(sub, all[1..3].to_vec(), "served from the cache");
    }

    #[test]
    fn incremental_parse_follows_edits() {
        let config = LANGUAGES.iter().find(|l| l.name == "rust").unwrap();
        let mut state = SyntaxState::new(config).unwrap();
        let mut text = Rope::from_str("fn a() {}\n");
        parse_now(&mut state, &text, 1);
        // Insert a second function after the first line.
        let inserted = "fn b() {}\n";
        text.insert(text.line_to_char(1), inserted);
        let edit = input_edit(&text, text.line_to_char(1), inserted, "");
        assert_eq!(edit.start_byte, 10);
        assert_eq!(edit.new_end_position, Point { row: 2, column: 0 });
        state.edited(&edit, 2);
        assert!(state.is_stale());
        // The edited old tree still yields highlights for untouched lines.
        let line0 = state.highlights(&text, 0..1);
        assert_eq!(
            line0[0].first().map(|span| span.token),
            Some(Token::Keyword)
        );
        parse_now(&mut state, &text, 2);
        let spans = state.highlights(&text, 1..2);
        assert_eq!(
            spans[0].first().map(|span| span.token),
            Some(Token::Keyword)
        );
        assert_eq!(state.parsed_version, 2);
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
