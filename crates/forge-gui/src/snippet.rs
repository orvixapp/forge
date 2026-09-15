//! LSP snippet syntax (`$1`, `${1:placeholder}`, `${1|a,b|}`, `$0`,
//! variables and escapes) expanded into plain text plus tabstop ranges.
//! Placeholders keep their default text, choices take the first option,
//! variables resolve to their default or nothing, so the editor can insert
//! the result as one transaction and select the first tabstop.

use std::ops::Range;

/// Expanded snippet: the text to insert and where each tabstop landed
/// (char offsets relative to the start of `text`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Snippet {
    pub text: String,
    /// Sorted by tabstop index with `$0` (the final cursor) last; a tabstop
    /// repeated in the snippet has several ranges.
    pub tabstops: Vec<Tabstop>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tabstop {
    pub index: u32,
    pub ranges: Vec<Range<usize>>,
}

impl Snippet {
    /// Whether accepting the snippet leaves anything to fill in.
    #[must_use]
    pub fn has_tabstops(&self) -> bool {
        self.tabstops.iter().any(|stop| stop.index != 0)
    }
}

/// Expands `source`. Never fails: malformed constructs are kept as text.
#[must_use]
pub fn parse(source: &str) -> Snippet {
    let mut parser = Parser {
        chars: source.chars().collect(),
        at: 0,
        text: String::new(),
        len: 0,
        stops: Vec::new(),
    };
    parser.parse_any(None);
    let mut tabstops: Vec<Tabstop> = Vec::new();
    for (index, range) in parser.stops {
        match tabstops.iter_mut().find(|stop| stop.index == index) {
            Some(stop) => stop.ranges.push(range),
            None => tabstops.push(Tabstop {
                index,
                ranges: vec![range],
            }),
        }
    }
    tabstops.sort_by_key(|stop| {
        if stop.index == 0 {
            u32::MAX
        } else {
            stop.index
        }
    });
    Snippet {
        text: parser.text,
        tabstops,
    }
}

struct Parser {
    chars: Vec<char>,
    at: usize,
    text: String,
    /// Chars emitted so far (`text.chars().count()` kept incrementally).
    len: usize,
    stops: Vec<(u32, Range<usize>)>,
}

impl Parser {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.at).copied()
    }

    fn peek_at(&self, offset: usize) -> Option<char> {
        self.chars.get(self.at + offset).copied()
    }

    fn emit(&mut self, c: char) {
        self.text.push(c);
        self.len += 1;
    }

    /// Parses until `until` (a closing `}`) or the end of input.
    fn parse_any(&mut self, until: Option<char>) {
        while let Some(c) = self.peek() {
            if Some(c) == until {
                return;
            }
            match c {
                '\\' => {
                    self.at += 1;
                    match self.peek() {
                        Some(escaped @ ('$' | '}' | '\\' | ',' | '|')) => {
                            self.at += 1;
                            self.emit(escaped);
                        }
                        _ => self.emit('\\'),
                    }
                }
                '$' => {
                    if !self.parse_dollar() {
                        self.at += 1;
                        self.emit('$');
                    }
                }
                _ => {
                    self.at += 1;
                    self.emit(c);
                }
            }
        }
    }

    /// At `$`; returns false when nothing valid follows so the caller keeps
    /// the dollar sign as text.
    fn parse_dollar(&mut self) -> bool {
        match self.peek_at(1) {
            Some(c) if c.is_ascii_digit() => {
                let start = self.at + 1;
                let mut end = start;
                while self.chars.get(end).is_some_and(char::is_ascii_digit) {
                    end += 1;
                }
                let index = number(&self.chars[start..end]);
                self.at = end;
                self.stops.push((index, self.len..self.len));
                true
            }
            Some(c) if is_variable_start(c) => {
                let start = self.at + 1;
                let mut end = start;
                while self.chars.get(end).is_some_and(|c| is_variable_char(*c)) {
                    end += 1;
                }
                let name: String = self.chars[start..end].iter().collect();
                self.at = end;
                self.emit_variable(&name, None);
                true
            }
            Some('{') => self.parse_braced(),
            _ => false,
        }
    }

    /// At `${`.
    fn parse_braced(&mut self) -> bool {
        let checkpoint = self.at;
        self.at += 2;
        let Some(first) = self.peek() else {
            self.at = checkpoint;
            return false;
        };
        if first.is_ascii_digit() {
            let start = self.at;
            while self.peek().is_some_and(|c| c.is_ascii_digit()) {
                self.at += 1;
            }
            let index = number(&self.chars[start..self.at]);
            let from = self.len;
            match self.peek() {
                Some('}') => {
                    self.at += 1;
                }
                Some(':') => {
                    self.at += 1;
                    self.parse_any(Some('}'));
                    if self.peek() == Some('}') {
                        self.at += 1;
                    }
                }
                Some('|') => {
                    self.at += 1;
                    self.parse_choice();
                }
                _ => {
                    self.at = checkpoint;
                    return false;
                }
            }
            self.stops.push((index, from..self.len));
            return true;
        }
        if is_variable_start(first) {
            let start = self.at;
            while self.peek().is_some_and(is_variable_char) {
                self.at += 1;
            }
            let name: String = self.chars[start..self.at].iter().collect();
            match self.peek() {
                Some('}') => {
                    self.at += 1;
                    self.emit_variable(&name, None);
                }
                Some(':') => {
                    self.at += 1;
                    let from = self.len;
                    let text_before = self.text.len();
                    self.parse_any(Some('}'));
                    if self.peek() == Some('}') {
                        self.at += 1;
                    }
                    if resolve_variable(&name).is_some() {
                        // Known variables replace their default.
                        self.text.truncate(text_before);
                        self.len = from;
                        self.emit_variable(&name, None);
                    }
                }
                Some('/') => {
                    // Transforms are not supported: skip to the closing brace
                    // and insert the plain variable.
                    while self.peek().is_some_and(|c| c != '}') {
                        self.at += 1;
                    }
                    if self.peek() == Some('}') {
                        self.at += 1;
                    }
                    self.emit_variable(&name, None);
                }
                _ => {
                    self.at = checkpoint;
                    return false;
                }
            }
            return true;
        }
        self.at = checkpoint;
        false
    }

    /// After `${n|`: keeps the first option, consumes through `|}`.
    fn parse_choice(&mut self) {
        let mut first = true;
        loop {
            match self.peek() {
                None => return,
                Some('\\') => {
                    self.at += 1;
                    if let Some(c) = self.peek() {
                        self.at += 1;
                        if first {
                            self.emit(c);
                        }
                    }
                }
                Some(',') => {
                    self.at += 1;
                    first = false;
                }
                Some('|') => {
                    self.at += 1;
                    if self.peek() == Some('}') {
                        self.at += 1;
                    }
                    return;
                }
                Some(c) => {
                    self.at += 1;
                    if first {
                        self.emit(c);
                    }
                }
            }
        }
    }

    fn emit_variable(&mut self, name: &str, _default: Option<&str>) {
        let value = resolve_variable(name).unwrap_or(name);
        for c in value.chars() {
            self.emit(c);
        }
    }
}

fn number(digits: &[char]) -> u32 {
    digits.iter().fold(0u32, |acc, c| {
        acc.saturating_mul(10)
            .saturating_add(c.to_digit(10).unwrap_or(0))
    })
}

fn is_variable_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_'
}

fn is_variable_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// Known snippet variables resolve to nothing (there is no selection or
/// clipboard context at insertion time); unknown names stay as text per
/// the specification.
fn resolve_variable(name: &str) -> Option<&'static str> {
    const KNOWN: [&str; 21] = [
        "TM_SELECTED_TEXT",
        "TM_CURRENT_LINE",
        "TM_CURRENT_WORD",
        "TM_LINE_INDEX",
        "TM_LINE_NUMBER",
        "TM_FILENAME",
        "TM_FILENAME_BASE",
        "TM_DIRECTORY",
        "TM_FILEPATH",
        "RELATIVE_FILEPATH",
        "CLIPBOARD",
        "WORKSPACE_NAME",
        "WORKSPACE_FOLDER",
        "CURRENT_YEAR",
        "CURRENT_MONTH",
        "CURRENT_DATE",
        "CURRENT_HOUR",
        "CURRENT_MINUTE",
        "CURRENT_SECOND",
        "RANDOM",
        "UUID",
    ];
    KNOWN.contains(&name).then_some("")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ranges(list: &[(usize, usize)]) -> Vec<Range<usize>> {
        list.iter().map(|(start, end)| *start..*end).collect()
    }

    fn stops(snippet: &Snippet) -> Vec<(u32, Vec<Range<usize>>)> {
        snippet
            .tabstops
            .iter()
            .map(|stop| (stop.index, stop.ranges.clone()))
            .collect()
    }

    #[test]
    fn placeholders_keep_defaults_and_report_ranges() {
        let snippet = parse("greet(${1:name})$0");
        assert_eq!(snippet.text, "greet(name)");
        assert_eq!(
            stops(&snippet),
            vec![(1, ranges(&[(6, 10)])), (0, ranges(&[(11, 11)]))]
        );
        assert!(snippet.has_tabstops());
    }

    #[test]
    fn repeated_tabstops_nested_placeholders_and_choices() {
        let snippet = parse("for ${1:item} in ${2:${1:item}s} { ${3|a,b\\,c|} }");
        assert_eq!(snippet.text, "for item in items { a }");
        assert_eq!(
            stops(&snippet),
            vec![
                (1, ranges(&[(4, 8), (12, 16)])),
                (2, ranges(&[(12, 17)])),
                (3, ranges(&[(20, 21)]))
            ]
        );
    }

    #[test]
    fn escapes_variables_and_malformed_input_stay_readable() {
        assert_eq!(parse(r"\$1 \\ \}").text, r"$1 \ }");
        assert_eq!(
            parse("${TM_FILENAME:x}${unknown}$other").text,
            "unknownother"
        );
        assert_eq!(parse("${TM_SELECTED_TEXT/(.*)/$1/}").text, "");
        let unterminated = parse("${1:abc");
        assert_eq!(unterminated.text, "abc");
        assert_eq!(parse("$").text, "$");
        assert_eq!(parse("${").text, "${");
        assert_eq!(parse("a😀${1:字}b").tabstops[0].ranges, ranges(&[(2, 3)]));
    }

    #[test]
    fn tabstops_are_ordered_with_final_cursor_last() {
        let snippet = parse("$2 $0 $1");
        assert_eq!(
            stops(&snippet),
            vec![
                (1, ranges(&[(2, 2)])),
                (2, ranges(&[(0, 0)])),
                (0, ranges(&[(1, 1)]))
            ]
        );
        assert!(!parse("plain $0").has_tabstops());
    }
}
