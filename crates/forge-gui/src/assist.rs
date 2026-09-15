//! Editor assistance without a language server (LSP is phase 5): syntax
//! errors straight from the tree-sitter tree, validation of Forge's own
//! config files against `Config`, and a completion popup fed by the words
//! of the buffer and, in config files, by the keys and values of the
//! config schema.

use forge_gui::config::Config;
use forge_syntax::SyntaxState;
use ropey::Rope;
use serde_json::Value;
use std::collections::BTreeSet;
use std::ops::Range;
use std::path::Path;
use std::sync::OnceLock;

/// Most syntax errors collected per buffer; enough to underline a screen.
const MAX_SYNTAX_ERRORS: usize = 200;
/// Rows shown in the completion popup.
pub const COMPLETION_ROWS: usize = 8;
/// Words shorter than this are neither indexed nor completed.
const MIN_WORD: usize = 3;
/// Buffers beyond this are not scanned for words (kept off the UI thread's
/// budget); the schema still completes.
const MAX_SCAN_BYTES: usize = 512 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticSeverity {
    Error,
    Warning,
    Information,
    Hint,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    /// Char range in the buffer; empty for a missing token.
    pub range: Range<usize>,
    pub message: String,
    pub severity: DiagnosticSeverity,
    pub source: Option<String>,
}

impl Diagnostic {
    #[must_use]
    pub fn error(range: Range<usize>, message: impl Into<String>) -> Self {
        Self {
            range,
            message: message.into(),
            severity: DiagnosticSeverity::Error,
            source: None,
        }
    }
}

/// Converts an LSP diagnostic to Forge's internal diagnostic structure.
#[must_use]
pub fn from_lsp_diagnostic(d: &lsp_types::Diagnostic, rope: &Rope) -> Diagnostic {
    let range = forge_lsp::sync::lsp_range_to_range(rope, &d.range);
    let severity = match d.severity {
        Some(lsp_types::DiagnosticSeverity::WARNING) => DiagnosticSeverity::Warning,
        Some(lsp_types::DiagnosticSeverity::INFORMATION) => DiagnosticSeverity::Information,
        Some(lsp_types::DiagnosticSeverity::HINT) => DiagnosticSeverity::Hint,
        _ => DiagnosticSeverity::Error,
    };
    Diagnostic {
        range,
        message: d.message.clone(),
        severity,
        source: d.source.clone(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionItem {
    pub label: String,
    /// Where it comes from: the section for a key, `value`, or `word`.
    pub detail: String,
    pub insert_text: Option<String>,
    pub documentation: Option<String>,
    /// Resolved char-range edits against the response's buffer revision.
    pub edits: Option<Vec<forge_buffer::Edit>>,
}

impl CompletionItem {
    #[must_use]
    pub fn new(label: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            detail: detail.into(),
            insert_text: None,
            documentation: None,
            edits: None,
        }
    }
}

/// Converts an LSP completion item to Forge's internal item.
#[must_use]
pub fn from_lsp_completion_item(item: &lsp_types::CompletionItem) -> CompletionItem {
    let detail = item.detail.clone().unwrap_or_else(|| match item.kind {
        Some(lsp_types::CompletionItemKind::FUNCTION) => "fn".to_string(),
        Some(lsp_types::CompletionItemKind::METHOD) => "method".to_string(),
        Some(lsp_types::CompletionItemKind::VARIABLE) => "var".to_string(),
        Some(lsp_types::CompletionItemKind::FIELD) => "field".to_string(),
        Some(lsp_types::CompletionItemKind::CLASS | lsp_types::CompletionItemKind::STRUCT) => {
            "type".to_string()
        }
        Some(lsp_types::CompletionItemKind::INTERFACE) => "trait".to_string(),
        Some(lsp_types::CompletionItemKind::MODULE) => "mod".to_string(),
        Some(lsp_types::CompletionItemKind::KEYWORD) => "keyword".to_string(),
        Some(lsp_types::CompletionItemKind::SNIPPET) => "snippet".to_string(),
        _ => "lsp".to_string(),
    });
    let documentation = item.documentation.as_ref().map(|doc| match doc {
        lsp_types::Documentation::String(s) => s.clone(),
        lsp_types::Documentation::MarkupContent(m) => m.value.clone(),
    });
    CompletionItem {
        label: item.label.clone(),
        detail,
        insert_text: item.insert_text.clone(),
        documentation,
        edits: None,
    }
}

#[derive(Debug, Clone)]
pub struct Completion {
    pub items: Vec<CompletionItem>,
    pub index: usize,
    /// Char index where the typed prefix starts.
    pub start: usize,
    pub prefix: String,
}

impl Completion {
    #[must_use]
    pub fn selected(&self) -> Option<&CompletionItem> {
        self.items.get(self.index)
    }

    /// First row shown so the selected item stays visible.
    #[must_use]
    pub fn first_visible(&self) -> usize {
        self.index.saturating_sub(COMPLETION_ROWS - 1)
    }
}

/// Keys and enum values of the config schema, flattened once.
pub struct SchemaHints {
    /// `(key, section)` — section is the dotted path that holds it.
    keys: Vec<(String, String)>,
    values: Vec<String>,
}

/// `config.toml` next to the user's config or under `.forge/`.
#[must_use]
pub fn is_forge_config(path: Option<&Path>) -> bool {
    let Some(path) = path else { return false };
    if path.file_name().and_then(|name| name.to_str()) != Some("config.toml") {
        return false;
    }
    path.parent()
        .and_then(|dir| dir.file_name())
        .and_then(|name| name.to_str())
        .is_some_and(|dir| dir == "forge" || dir == ".forge")
}

pub fn schema_hints() -> &'static SchemaHints {
    static HINTS: OnceLock<SchemaHints> = OnceLock::new();
    HINTS.get_or_init(|| {
        let schema: Value = serde_json::from_str(&Config::json_schema()).unwrap_or(Value::Null);
        let mut hints = SchemaHints {
            keys: Vec::new(),
            values: Vec::new(),
        };
        let defs = schema.get("$defs").cloned().unwrap_or(Value::Null);
        collect_schema(&schema, &defs, "", &mut hints, 0);
        hints.keys.sort();
        hints.keys.dedup();
        hints.values.sort();
        hints.values.dedup();
        hints
    })
}

fn collect_schema(node: &Value, defs: &Value, path: &str, hints: &mut SchemaHints, depth: usize) {
    if depth > 8 {
        return;
    }
    if let Some(reference) = node.get("$ref").and_then(Value::as_str)
        && let Some(name) = reference.strip_prefix("#/$defs/")
        && let Some(target) = defs.get(name)
    {
        collect_schema(target, defs, path, hints, depth + 1);
        return;
    }
    if let Some(values) = node.get("enum").and_then(Value::as_array) {
        hints
            .values
            .extend(values.iter().filter_map(Value::as_str).map(str::to_owned));
    }
    // A documented variant becomes `const` instead of an `enum` entry.
    if let Some(value) = node.get("const").and_then(Value::as_str) {
        hints.values.push(value.to_owned());
    }
    if let Some(properties) = node.get("properties").and_then(Value::as_object) {
        for (key, child) in properties {
            hints.keys.push((key.clone(), path.to_owned()));
            let child_path = if path.is_empty() {
                key.clone()
            } else {
                format!("{path}.{key}")
            };
            collect_schema(child, defs, &child_path, hints, depth + 1);
        }
    }
    for branch in ["items", "additionalProperties"] {
        if let Some(child) = node.get(branch).filter(|v| v.is_object()) {
            collect_schema(child, defs, path, hints, depth + 1);
        }
    }
    for list in ["anyOf", "oneOf", "allOf"] {
        for child in node
            .get(list)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            collect_schema(child, defs, path, hints, depth + 1);
        }
    }
}

/// `ERROR`/`MISSING` nodes of the current tree as char ranges.
#[must_use]
pub fn syntax_diagnostics(state: &SyntaxState, rope: &Rope) -> Vec<Diagnostic> {
    state
        .syntax_errors(MAX_SYNTAX_ERRORS)
        .into_iter()
        .map(|error| {
            let start = rope.byte_to_char(error.bytes.start.min(rope.len_bytes()));
            let end = rope.byte_to_char(error.bytes.end.min(rope.len_bytes()));
            let message = match error.expected {
                Some(expected) => format!("syntax: missing {expected}"),
                None => "syntax error".to_owned(),
            };
            Diagnostic::error(start..end.max(start), message)
        })
        .collect()
}

/// What `Config` says about `text` (a Forge config file): at most one
/// error, at the span TOML reports.
#[must_use]
pub fn config_diagnostics(text: &str, rope: &Rope) -> Vec<Diagnostic> {
    let Err(error) = toml::from_str::<Config>(text) else {
        return Vec::new();
    };
    let bytes = error.span().unwrap_or(0..0);
    let start = rope.byte_to_char(bytes.start.min(rope.len_bytes()));
    let end = rope.byte_to_char(bytes.end.min(rope.len_bytes()));
    // toml's message repeats the location on its own lines; keep the gist.
    let message = error
        .message()
        .lines()
        .next()
        .unwrap_or("invalid configuration")
        .trim()
        .to_owned();
    vec![Diagnostic::error(start..end.max(start), message)]
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// The identifier characters right before `cursor`.
#[must_use]
pub fn word_prefix(rope: &Rope, cursor: usize) -> (usize, String) {
    let cursor = cursor.min(rope.len_chars());
    let mut start = cursor;
    while start > 0 && is_word_char(rope.char(start - 1)) {
        start -= 1;
    }
    (start, rope.slice(start..cursor).to_string())
}

/// Builds the popup for the primary cursor, or `None` when nothing fits.
/// `explicit` (Ctrl+Space) allows an empty prefix; automatic triggering
/// waits for two characters.
#[must_use]
pub fn complete(
    rope: &Rope,
    cursor: usize,
    explicit: bool,
    hints: Option<&SchemaHints>,
    language: Option<&str>,
) -> Option<Completion> {
    let (start, prefix) = word_prefix(rope, cursor);
    if !explicit && prefix.chars().count() < 2 {
        return None;
    }
    let lower = prefix.to_lowercase();
    let matches = |candidate: &str| {
        let candidate = candidate.to_lowercase();
        candidate != lower && (lower.is_empty() || candidate.starts_with(&lower))
    };
    let mut items: Vec<CompletionItem> = Vec::new();
    items.extend(
        language
            .into_iter()
            .flat_map(language_keywords)
            .filter(|keyword| matches(keyword))
            .map(|keyword| CompletionItem::new(*keyword, "keyword")),
    );
    if let Some(hints) = hints {
        // Before `=` on the line a key is being typed; after it, a value.
        let line = rope.char_to_line(start);
        let line_start = rope.line_to_char(line);
        let before: String = rope.slice(line_start..start).to_string();
        if before.contains('=') {
            items.extend(
                hints
                    .values
                    .iter()
                    .filter(|value| matches(value))
                    .map(|value| CompletionItem::new(value.clone(), "value")),
            );
        } else {
            items.extend(hints.keys.iter().filter(|(key, _)| matches(key)).map(
                |(key, section)| {
                    CompletionItem::new(
                        key.clone(),
                        if section.is_empty() {
                            "key".to_owned()
                        } else {
                            format!("[{section}]")
                        },
                    )
                },
            ));
        }
    }
    if rope.len_bytes() <= MAX_SCAN_BYTES {
        let known: BTreeSet<&str> = items.iter().map(|item| item.label.as_str()).collect();
        let mut words: BTreeSet<String> = BTreeSet::new();
        let text = rope.to_string();
        for word in text
            .split(|c: char| !is_word_char(c))
            .filter(|word| word.chars().count() >= MIN_WORD)
        {
            if matches(word) && !known.contains(word) && !word.chars().all(|c| c.is_ascii_digit()) {
                words.insert(word.to_owned());
            }
        }
        items.extend(
            words
                .into_iter()
                .map(|label| CompletionItem::new(label, "word")),
        );
    }
    // Exact-case prefix matches first, then the rest, stable otherwise.
    items.sort_by_key(|item| !item.label.starts_with(&prefix));
    items.truncate(64);
    (!items.is_empty()).then_some(Completion {
        items,
        index: 0,
        start,
        prefix,
    })
}

fn language_keywords(language: &str) -> &'static [&'static str] {
    match language {
        "rust" => &[
            "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum",
            "extern", "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod",
            "move", "mut", "pub", "ref", "return", "self", "Self", "static", "struct", "super",
            "trait", "true", "type", "unsafe", "use", "where", "while",
        ],
        "toml" => &["true", "false"],
        "json" => &["true", "false", "null"],
        "bash" => &[
            "case", "do", "done", "elif", "else", "esac", "export", "fi", "for", "function", "if",
            "in", "local", "readonly", "select", "then", "until", "while",
        ],
        "python" => &[
            "and", "as", "assert", "async", "await", "break", "class", "continue", "def", "del",
            "elif", "else", "except", "False", "finally", "for", "from", "global", "if", "import",
            "in", "is", "lambda", "None", "nonlocal", "not", "or", "pass", "raise", "return",
            "True", "try", "while", "with", "yield",
        ],
        "javascript" => &[
            "async",
            "await",
            "break",
            "case",
            "catch",
            "class",
            "const",
            "continue",
            "debugger",
            "default",
            "delete",
            "do",
            "else",
            "export",
            "extends",
            "false",
            "finally",
            "for",
            "from",
            "function",
            "if",
            "import",
            "in",
            "instanceof",
            "let",
            "new",
            "null",
            "of",
            "return",
            "static",
            "super",
            "switch",
            "this",
            "throw",
            "true",
            "try",
            "typeof",
            "undefined",
            "var",
            "void",
            "while",
            "yield",
        ],
        "c" => &[
            "auto", "break", "case", "char", "const", "continue", "default", "do", "double",
            "else", "enum", "extern", "float", "for", "goto", "if", "inline", "int", "long",
            "register", "restrict", "return", "short", "signed", "sizeof", "static", "struct",
            "switch", "typedef", "union", "unsigned", "void", "volatile", "while",
        ],
        _ => &[],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forge_config_paths_are_recognised() {
        assert!(is_forge_config(Some(Path::new(
            "/home/x/.config/forge/config.toml"
        ))));
        assert!(is_forge_config(Some(Path::new("/repo/.forge/config.toml"))));
        assert!(!is_forge_config(Some(Path::new("/repo/Cargo.toml"))));
        assert!(!is_forge_config(Some(Path::new("/repo/config.toml"))));
        assert!(!is_forge_config(None));
    }

    #[test]
    fn schema_hints_know_sections_keys_and_enums() {
        let hints = schema_hints();
        assert!(
            hints
                .keys
                .iter()
                .any(|(key, section)| key == "language" && section == "ui")
        );
        assert!(
            hints
                .keys
                .iter()
                .any(|(key, section)| key == "model" && section == "providers")
        );
        assert!(hints.keys.iter().any(|(key, _)| key == "router"));
        assert!(
            hints
                .values
                .iter()
                .any(|value| value == "openai-compatible")
        );
        assert!(hints.values.iter().any(|value| value == "spanish"));
    }

    #[test]
    fn completion_prefers_schema_keys_then_words() {
        let rope = Rope::from_str("[ui]\nlang\n# language_hint = 1\n");
        let cursor = rope.line_to_char(1) + 4;
        let completion =
            complete(&rope, cursor, false, Some(schema_hints()), Some("toml")).unwrap();
        assert_eq!(completion.prefix, "lang");
        assert_eq!(completion.items[0].label, "language");
        assert_eq!(completion.items[0].detail, "[ui]");
        assert!(
            completion
                .items
                .iter()
                .any(|item| item.label == "language_hint")
        );
        // Values after `=`.
        let rope = Rope::from_str("[ui]\nlanguage = spa\n");
        let cursor = rope.len_chars() - 1;
        let completion =
            complete(&rope, cursor, false, Some(schema_hints()), Some("toml")).unwrap();
        assert_eq!(completion.items[0].label, "spanish");
        assert_eq!(completion.items[0].detail, "value");
        // Too short unless explicit.
        let rope = Rope::from_str("fn compute() {}\nc");
        assert!(complete(&rope, rope.len_chars(), false, None, Some("rust")).is_none());
        let explicit = complete(&rope, rope.len_chars(), true, None, Some("rust")).unwrap();
        assert!(explicit.items.iter().any(|item| item.label == "compute"));
    }

    #[test]
    fn every_programming_language_offers_its_keywords() {
        for (language, prefix, expected) in [
            ("rust", "str", "struct"),
            ("toml", "tru", "true"),
            ("json", "nul", "null"),
            ("bash", "exp", "export"),
            ("python", "asy", "async"),
            ("javascript", "fun", "function"),
            ("c", "typ", "typedef"),
        ] {
            let rope = Rope::from_str(prefix);
            let completion = complete(&rope, rope.len_chars(), false, None, Some(language))
                .unwrap_or_else(|| panic!("no completion for {language}"));
            assert!(
                completion.items.iter().any(|item| item.label == expected),
                "{language} did not suggest {expected}"
            );
        }
    }

    #[test]
    fn config_diagnostics_point_at_the_offending_key() {
        let text = "[ui]\nlangauge = \"spanish\"\n";
        let rope = Rope::from_str(text);
        let diagnostics = config_diagnostics(text, &rope);
        assert_eq!(diagnostics.len(), 1);
        assert!(
            diagnostics[0].message.contains("langauge"),
            "{}",
            diagnostics[0].message
        );
        assert_eq!(diagnostics[0].range.start, rope.line_to_char(1));
        assert!(
            config_diagnostics("[ui]\nlanguage = \"english\"\n", &Rope::from_str("x")).is_empty()
        );
    }
}
