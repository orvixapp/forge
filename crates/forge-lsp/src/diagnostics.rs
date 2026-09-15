//! High-performance diagnostic store with line indexing for 50k+ diagnostics without frame drops.

use lsp_types::{Diagnostic, DiagnosticSeverity, Uri};
use std::collections::{BTreeMap, HashMap};
use std::ops::Range;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

/// Diagnostics associated with a single document, indexed by line.
#[derive(Debug, Clone)]
pub struct DocumentDiagnostics {
    pub uri: Uri,
    pub version: Option<i32>,
    pub items: Vec<Diagnostic>,
    /// Line index (0-indexed) -> list of indices into `items`.
    pub line_index: BTreeMap<u32, Vec<usize>>,
    long_ranges: Vec<usize>,
    errors: usize,
    warnings: usize,
}

impl DocumentDiagnostics {
    #[must_use]
    pub fn new(uri: Uri, version: Option<i32>, items: Vec<Diagnostic>) -> Self {
        let mut line_index: BTreeMap<u32, Vec<usize>> = BTreeMap::new();
        let mut long_ranges = Vec::new();
        for (idx, diag) in items.iter().enumerate() {
            let start_line = diag.range.start.line;
            let end_line = if diag.range.end.character == 0 && diag.range.end.line > start_line {
                diag.range.end.line - 1
            } else {
                diag.range.end.line
            };
            if end_line.saturating_sub(start_line) > 64 {
                long_ranges.push(idx);
                continue;
            }
            for line in start_line..=end_line {
                line_index.entry(line).or_default().push(idx);
            }
        }

        Self {
            errors: items
                .iter()
                .filter(|diag| {
                    diag.severity.unwrap_or(DiagnosticSeverity::ERROR) == DiagnosticSeverity::ERROR
                })
                .count(),
            warnings: items
                .iter()
                .filter(|diag| diag.severity == Some(DiagnosticSeverity::WARNING))
                .count(),
            uri,
            version,
            items,
            line_index,
            long_ranges,
        }
    }

    /// Diagnostics that intersect the given line.
    #[must_use]
    pub fn for_line(&self, line: u32) -> Vec<&Diagnostic> {
        let mut result: Vec<_> = self
            .line_index
            .get(&line)
            .map(|indices| indices.iter().map(|&i| &self.items[i]).collect())
            .unwrap_or_default();
        result.extend(
            self.long_ranges
                .iter()
                .map(|&index| &self.items[index])
                .filter(|diag| diag.range.start.line <= line && diag.range.end.line >= line),
        );
        result
    }

    /// Diagnostics that intersect any line in the given range.
    #[must_use]
    pub fn for_line_range(&self, lines: Range<u32>) -> Vec<&Diagnostic> {
        let mut seen = std::collections::HashSet::new();
        let mut result = Vec::new();

        for (_, indices) in self.line_index.range(lines.clone()) {
            for &idx in indices {
                if seen.insert(idx) {
                    result.push(&self.items[idx]);
                }
            }
        }
        result.extend(
            self.long_ranges
                .iter()
                .map(|&index| &self.items[index])
                .filter(|diag| {
                    diag.range.start.line < lines.end && diag.range.end.line >= lines.start
                }),
        );
        result
    }

    /// Count of items with severity Error.
    #[must_use]
    pub fn error_count(&self) -> usize {
        self.errors
    }

    /// Count of items with severity Warning.
    #[must_use]
    pub fn warning_count(&self) -> usize {
        self.warnings
    }
}

/// Diagnostics of one document as reported by one source (`server/push`,
/// `server/pull`), so servers and transports never overwrite each other.
#[derive(Debug, Clone)]
struct SourceDiagnostics {
    version: Option<i32>,
    items: Vec<Diagnostic>,
}

#[derive(Debug, Default)]
struct StoreInner {
    sources: HashMap<Uri, HashMap<String, SourceDiagnostics>>,
    merged: HashMap<Uri, DocumentDiagnostics>,
}

impl StoreInner {
    fn rebuild(&mut self, uri: &Uri) {
        let Some(sources) = self.sources.get(uri) else {
            self.merged.remove(uri);
            return;
        };
        if sources.is_empty() {
            self.sources.remove(uri);
            self.merged.remove(uri);
            return;
        }
        let version = sources.values().filter_map(|source| source.version).max();
        let items: Vec<Diagnostic> = sources
            .values()
            .flat_map(|source| source.items.iter().cloned())
            .collect();
        self.merged.insert(
            uri.clone(),
            DocumentDiagnostics::new(uri.clone(), version, items),
        );
    }
}

/// Global diagnostic store indexed by document URI and lines, merged across
/// sources (push and pull of every server).
#[derive(Debug, Clone, Default)]
pub struct DiagnosticStore {
    inner: Arc<RwLock<StoreInner>>,
    generation: Arc<AtomicU64>,
}

impl DiagnosticStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Updates diagnostics for a document from an anonymous source
    /// (`textDocument/publishDiagnostics` of a single server).
    pub fn update(&self, uri: &Uri, version: Option<i32>, items: Vec<Diagnostic>) {
        self.update_from(uri, "push", version, items);
    }

    /// Updates the diagnostics one `source` reports for a document. A
    /// report older than the source's current one is ignored.
    pub fn update_from(
        &self,
        uri: &Uri,
        source: &str,
        version: Option<i32>,
        items: Vec<Diagnostic>,
    ) {
        let mut guard = self
            .inner
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if guard
            .sources
            .get(uri)
            .and_then(|sources| sources.get(source))
            .is_some_and(|old| old.version.zip(version).is_some_and(|(old, new)| new < old))
        {
            return;
        }
        guard
            .sources
            .entry(uri.clone())
            .or_default()
            .insert(source.to_owned(), SourceDiagnostics { version, items });
        guard.rebuild(uri);
        self.generation.fetch_add(1, Ordering::Relaxed);
    }

    /// Drops what one source reported for a document (pull results on close).
    pub fn clear_source(&self, uri: &Uri, source: &str) {
        let mut guard = self
            .inner
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let removed = guard
            .sources
            .get_mut(uri)
            .is_some_and(|sources| sources.remove(source).is_some());
        if removed {
            guard.rebuild(uri);
            self.generation.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Clears diagnostics for a document (e.g. on close or deletion).
    pub fn clear(&self, uri: &Uri) {
        let mut guard = self
            .inner
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.sources.remove(uri);
        guard.merged.remove(uri);
        self.generation.fetch_add(1, Ordering::Relaxed);
    }

    /// Clears all diagnostics.
    pub fn clear_all(&self) {
        let mut guard = self
            .inner
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.sources.clear();
        guard.merged.clear();
        self.generation.fetch_add(1, Ordering::Relaxed);
    }

    /// Bumps on every change; cheap way for pollers to skip unchanged state.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }

    /// Newest published document revision across sources, if any supplied one.
    #[must_use]
    pub fn version_for_document(&self, uri: &Uri) -> Option<i32> {
        self.inner
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .merged
            .get(uri)
            .and_then(|document| document.version)
    }

    /// Returns all diagnostics for a specific line in a document.
    #[must_use]
    pub fn for_line(&self, uri: &Uri, line: u32) -> Vec<Diagnostic> {
        let guard = self
            .inner
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard
            .merged
            .get(uri)
            .map(|doc| doc.for_line(line).into_iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Returns all diagnostics for a line range (e.g. visible viewport).
    #[must_use]
    pub fn for_line_range(&self, uri: &Uri, lines: Range<u32>) -> Vec<Diagnostic> {
        let guard = self
            .inner
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard
            .merged
            .get(uri)
            .map(|doc| doc.for_line_range(lines).into_iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Returns all diagnostics for a document.
    #[must_use]
    pub fn for_document(&self, uri: &Uri) -> Vec<Diagnostic> {
        let guard = self
            .inner
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard
            .merged
            .get(uri)
            .map(|doc| doc.items.clone())
            .unwrap_or_default()
    }

    /// Returns counts: `(errors, warnings, total)` for a document.
    #[must_use]
    pub fn counts_for_document(&self, uri: &Uri) -> (usize, usize, usize) {
        let guard = self
            .inner
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.merged.get(uri).map_or((0, 0, 0), |doc| {
            (doc.error_count(), doc.warning_count(), doc.items.len())
        })
    }

    /// Returns total counts: `(errors, warnings, total)` across all documents.
    #[must_use]
    pub fn total_counts(&self) -> (usize, usize, usize) {
        let guard = self
            .inner
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut errors = 0;
        let mut warnings = 0;
        let mut total = 0;
        for doc in guard.merged.values() {
            errors += doc.error_count();
            warnings += doc.warning_count();
            total += doc.items.len();
        }
        (errors, warnings, total)
    }

    /// Returns all documents with their diagnostics (for MCP tools or Problems panel).
    #[must_use]
    pub fn all_diagnostics(&self) -> Vec<(Uri, Vec<Diagnostic>)> {
        let guard = self
            .inner
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard
            .merged
            .iter()
            .map(|(uri, doc)| (uri.clone(), doc.items.clone()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsp_types::{Position, Range};
    use std::str::FromStr;

    #[test]
    fn test_diagnostic_store_indexing() {
        let store = DiagnosticStore::new();
        let uri = Uri::from_str("file:///project/src/lib.rs").unwrap();

        let diag1 = Diagnostic {
            range: Range {
                start: Position {
                    line: 5,
                    character: 0,
                },
                end: Position {
                    line: 5,
                    character: 10,
                },
            },
            severity: Some(DiagnosticSeverity::ERROR),
            message: "mismatched types".to_string(),
            ..Default::default()
        };

        let diag2 = Diagnostic {
            range: Range {
                start: Position {
                    line: 5,
                    character: 4,
                },
                end: Position {
                    line: 7,
                    character: 2,
                },
            },
            severity: Some(DiagnosticSeverity::WARNING),
            message: "unused variable".to_string(),
            ..Default::default()
        };

        store.update(&uri, Some(1), vec![diag1, diag2]);

        // Line 5 should have 2 diagnostics
        let line5 = store.for_line(&uri, 5);
        assert_eq!(line5.len(), 2);

        // Line 6 should have diag2 (multiline)
        let line6 = store.for_line(&uri, 6);
        assert_eq!(line6.len(), 1);
        assert_eq!(line6[0].message, "unused variable");

        // Line 10 should have 0 diagnostics
        let line10 = store.for_line(&uri, 10);
        assert!(line10.is_empty());

        // Range 5..7
        let range_diags = store.for_line_range(&uri, 5..7);
        assert_eq!(range_diags.len(), 2);

        let (errs, warns, total) = store.counts_for_document(&uri);
        assert_eq!(errs, 1);
        assert_eq!(warns, 1);
        assert_eq!(total, 2);
    }

    #[test]
    fn sources_merge_without_overwriting_each_other() {
        let store = DiagnosticStore::new();
        let uri = Uri::from_str("file:///project/src/lib.rs").unwrap();
        let diag = |message: &str| Diagnostic {
            range: Range::new(Position::new(0, 0), Position::new(0, 1)),
            severity: Some(DiagnosticSeverity::ERROR),
            message: message.to_string(),
            ..Default::default()
        };
        store.update_from(&uri, "ra/push", Some(3), vec![diag("cargo check")]);
        store.update_from(&uri, "ra/pull", Some(4), vec![diag("native")]);
        assert_eq!(store.counts_for_document(&uri), (2, 0, 2));
        assert_eq!(store.version_for_document(&uri), Some(4));
        // A stale pull report for the same source is dropped.
        store.update_from(&uri, "ra/pull", Some(2), Vec::new());
        assert_eq!(store.counts_for_document(&uri), (2, 0, 2));
        // Dropping one source keeps the other; dropping the last removes the document.
        store.clear_source(&uri, "ra/pull");
        assert_eq!(store.for_document(&uri)[0].message, "cargo check");
        store.update_from(&uri, "ra/push", Some(5), Vec::new());
        assert_eq!(store.counts_for_document(&uri), (0, 0, 0));
        store.clear_source(&uri, "ra/push");
        assert!(store.all_diagnostics().is_empty());
    }

    #[test]
    fn test_benchmark_50k_diagnostics_scale() {
        let store = DiagnosticStore::new();
        let uri = Uri::from_str("file:///big/file.rs").unwrap();

        let mut items = Vec::with_capacity(50_000);
        for i in 0u32..50_000 {
            let line = i % 5000;
            items.push(Diagnostic {
                range: Range {
                    start: Position { line, character: 0 },
                    end: Position { line, character: 5 },
                },
                severity: Some(DiagnosticSeverity::ERROR),
                message: format!("synthetic error {i}"),
                ..Default::default()
            });
        }

        let start = std::time::Instant::now();
        store.update(&uri, Some(1), items);
        let update_elapsed = start.elapsed();
        // Should index 50k diagnostics in tens of milliseconds
        assert!(update_elapsed.as_millis() < 500);

        let start = std::time::Instant::now();
        let line_diags = store.for_line(&uri, 42);
        let query_elapsed = start.elapsed();
        // Querying a line must be sub-millisecond (< 100 microseconds typically)
        assert!(query_elapsed.as_micros() < 1000);
        assert!(!line_diags.is_empty());
    }
}
