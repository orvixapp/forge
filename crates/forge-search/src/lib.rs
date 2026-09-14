//! Text search (ARCHITECTURE.md §13.6): regex over a buffer's rope, and
//! project-wide search with the ripgrep crates streamed through a channel
//! so the UI can show the first results while the walk continues.

use forge_project::walker;
use grep_matcher::Matcher as _;
use grep_regex::RegexMatcherBuilder;
use grep_searcher::{BinaryDetection, SearcherBuilder, sinks::UTF8};
use ropey::Rope;
use std::{
    ops::Range,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc::{self, Receiver},
    },
    thread,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SearchOptions {
    pub regex: bool,
    pub case_sensitive: bool,
    pub whole_word: bool,
}

/// Compiles `query` according to `options`.
///
/// # Errors
///
/// An invalid regular expression, described for the user.
pub fn compile(query: &str, options: SearchOptions) -> Result<regex::Regex, String> {
    let mut pattern = if options.regex {
        query.to_owned()
    } else {
        regex::escape(query)
    };
    if options.whole_word {
        pattern = format!(r"\b(?:{pattern})\b");
    }
    regex::RegexBuilder::new(&pattern)
        .case_insensitive(!options.case_sensitive)
        .multi_line(true)
        .size_limit(1 << 20)
        .build()
        .map_err(|error| format!("expresión regular inválida: {error}"))
}

/// Matches of `pattern` in `text` as char ranges, in order.
#[must_use]
pub fn search_rope(text: &Rope, pattern: &regex::Regex, limit: usize) -> Vec<Range<usize>> {
    // The regex crate needs a contiguous haystack; a rope of a few MB is
    // copied in a millisecond, which is fine for a find bar. Streaming
    // over chunks with regex-automata is the large-file follow-up.
    let haystack = text.to_string();
    pattern
        .find_iter(&haystack)
        .filter(|found| !found.is_empty())
        .take(limit)
        .map(|found| text.byte_to_char(found.start())..text.byte_to_char(found.end()))
        .collect()
}

/// One matching line of a file in the project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectMatch {
    pub path: PathBuf,
    /// 1-based.
    pub line: u64,
    /// Byte range of the first match inside `text`.
    pub range: Range<usize>,
    pub text: String,
}

/// Events streamed by [`search_project`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchEvent {
    Match(ProjectMatch),
    /// The walk finished; `truncated` when the result cap stopped it.
    Done {
        truncated: bool,
        files: usize,
    },
    Error(String),
}

/// Handle to a running project search; dropping it cancels the search.
pub struct ProjectSearch {
    pub events: Receiver<SearchEvent>,
    cancel: Arc<AtomicBool>,
}

impl Drop for ProjectSearch {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// Searches every non-excluded file under `root` on background threads.
/// At most `limit` matches are reported.
///
/// # Errors
///
/// An invalid pattern or exclusion glob; nothing is spawned then.
pub fn search_project(
    root: &Path,
    query: &str,
    options: SearchOptions,
    limit: usize,
) -> Result<ProjectSearch, String> {
    let mut pattern = if options.regex {
        query.to_owned()
    } else {
        regex::escape(query)
    };
    if options.whole_word {
        pattern = format!(r"\b(?:{pattern})\b");
    }
    let matcher = RegexMatcherBuilder::new()
        .case_insensitive(!options.case_sensitive)
        .build(&pattern)
        .map_err(|error| format!("expresión regular inválida: {error}"))?;
    let walker = walker(root, &[]).map_err(|error| error.to_string())?;
    let (tx, events) = mpsc::channel();
    let cancel = Arc::new(AtomicBool::new(false));
    let handle = ProjectSearch {
        events,
        cancel: Arc::clone(&cancel),
    };
    let root = root.to_path_buf();
    thread::Builder::new()
        .name("forge-search".into())
        .spawn(move || {
            let count = Arc::new(AtomicUsize::new(0));
            let files = Arc::new(AtomicUsize::new(0));
            let truncated = Arc::new(AtomicBool::new(false));
            walker.build_parallel().run(|| {
                let tx = tx.clone();
                let matcher = matcher.clone();
                let cancel = Arc::clone(&cancel);
                let count = Arc::clone(&count);
                let files = Arc::clone(&files);
                let truncated = Arc::clone(&truncated);
                let root = root.clone();
                let mut searcher = SearcherBuilder::new()
                    .binary_detection(BinaryDetection::quit(b'\x00'))
                    .line_number(true)
                    .build();
                Box::new(move |entry| {
                    if cancel.load(Ordering::Relaxed) || truncated.load(Ordering::Relaxed) {
                        return ignore::WalkState::Quit;
                    }
                    let Ok(entry) = entry else {
                        return ignore::WalkState::Continue;
                    };
                    if !entry.file_type().is_some_and(|kind| kind.is_file()) {
                        return ignore::WalkState::Continue;
                    }
                    files.fetch_add(1, Ordering::Relaxed);
                    let path = entry.path().to_path_buf();
                    let relative = path
                        .strip_prefix(&root)
                        .map_or_else(|_| path.clone(), Path::to_path_buf);
                    let sink = UTF8(|line, text| {
                        if count.fetch_add(1, Ordering::Relaxed) >= limit {
                            truncated.store(true, Ordering::Relaxed);
                            return Ok(false);
                        }
                        let range = matcher
                            .find(text.as_bytes())
                            .ok()
                            .flatten()
                            .map_or(0..0, |found| found.start()..found.end());
                        let _ = tx.send(SearchEvent::Match(ProjectMatch {
                            path: relative.clone(),
                            line,
                            range,
                            text: text.trim_end_matches(['\n', '\r']).to_owned(),
                        }));
                        Ok(!cancel.load(Ordering::Relaxed))
                    });
                    let _ = searcher.search_path(&matcher, &path, sink);
                    ignore::WalkState::Continue
                })
            });
            let _ = tx.send(SearchEvent::Done {
                truncated: truncated.load(Ordering::Relaxed),
                files: files.load(Ordering::Relaxed),
            });
        })
        .map_err(|error| error.to_string())?;
    Ok(handle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rope_search_returns_char_ranges() {
        let text = Rope::from_str("héllo hello\nHELLO");
        let pattern = compile("hello", SearchOptions::default()).unwrap();
        assert_eq!(search_rope(&text, &pattern, 10), [6..11, 12..17]);
        let sensitive = compile(
            "hello",
            SearchOptions {
                case_sensitive: true,
                ..SearchOptions::default()
            },
        )
        .unwrap();
        assert_eq!(search_rope(&text, &sensitive, 10), vec![6..11]);
        let word = compile(
            "ell",
            SearchOptions {
                whole_word: true,
                ..SearchOptions::default()
            },
        )
        .unwrap();
        assert!(search_rope(&text, &word, 10).is_empty());
        assert!(
            compile(
                "(",
                SearchOptions {
                    regex: true,
                    ..SearchOptions::default()
                }
            )
            .is_err()
        );
    }

    #[test]
    fn project_search_streams_matches_and_respects_the_cap() {
        let dir = std::env::temp_dir().join(format!("forge-search-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join("node_modules")).unwrap();
        std::fs::write(dir.join("src/a.rs"), "needle one\nnothing\nNEEDLE two\n").unwrap();
        std::fs::write(dir.join("src/b.txt"), "needle three\n").unwrap();
        std::fs::write(dir.join("node_modules/c.js"), "needle hidden\n").unwrap();
        let search = search_project(&dir, "needle", SearchOptions::default(), 100).unwrap();
        let events: Vec<SearchEvent> = search.events.iter().collect();
        let mut matches: Vec<&ProjectMatch> = events
            .iter()
            .filter_map(|event| match event {
                SearchEvent::Match(found) => Some(found),
                _ => None,
            })
            .collect();
        matches.sort_by_key(|found| (found.path.clone(), found.line));
        let summary: Vec<(String, u64, Range<usize>)> = matches
            .iter()
            .map(|found| {
                (
                    found.path.display().to_string(),
                    found.line,
                    found.range.clone(),
                )
            })
            .collect();
        assert_eq!(
            summary,
            [
                ("src/a.rs".to_owned(), 1, 0..6),
                ("src/a.rs".to_owned(), 3, 0..6),
                ("src/b.txt".to_owned(), 1, 0..6)
            ]
        );
        assert!(matches!(
            events.last(),
            Some(SearchEvent::Done {
                truncated: false,
                files: 2
            })
        ));
        let capped = search_project(&dir, "needle", SearchOptions::default(), 1).unwrap();
        let events: Vec<SearchEvent> = capped.events.iter().collect();
        let count = events
            .iter()
            .filter(|event| matches!(event, SearchEvent::Match(_)))
            .count();
        assert_eq!(count, 1);
        assert!(matches!(
            events.last(),
            Some(SearchEvent::Done {
                truncated: true,
                ..
            })
        ));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
