//! The workspace as the editor sees it (ARCHITECTURE.md §13.6): an
//! ignore-aware walk of the root, an in-memory path index with fuzzy
//! matching, and a file watcher for open buffers.

use ignore::{WalkBuilder, overrides::OverrideBuilder};
use nucleo_matcher::{
    Config, Matcher, Utf32Str,
    pattern::{CaseMatching, Normalization, Pattern},
};
use std::{
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver},
    time::Instant,
};

/// Directories and globs never scanned or searched by default. Files inside
/// stay openable by path (§13.6).
pub const GLOBAL_EXCLUDES: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    "vendor",
    "build",
    "dist",
    "out",
    ".cache",
    "__pycache__",
    ".venv",
    "venv",
    ".gradle",
    ".idea",
    ".next",
    ".turbo",
    "coverage",
    "*.min.js",
    "*.map",
];

/// A walker over `root` honouring `.gitignore`, `.ignore`, hidden-file
/// rules and [`GLOBAL_EXCLUDES`]; shared by the index and project search.
///
/// # Errors
///
/// Invalid exclusion globs (never for the built-in list).
pub fn walker(root: &Path, extra_excludes: &[String]) -> Result<WalkBuilder, ignore::Error> {
    let mut overrides = OverrideBuilder::new(root);
    for pattern in GLOBAL_EXCLUDES
        .iter()
        .map(|pattern| (*pattern).to_owned())
        .chain(extra_excludes.iter().cloned())
    {
        // Override globs are inclusions unless negated.
        overrides.add(&format!("!{pattern}"))?;
    }
    let mut builder = WalkBuilder::new(root);
    builder
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        // `.gitignore` files count even outside a git checkout.
        .require_git(false)
        .ignore(true)
        .follow_links(false)
        .overrides(overrides.build()?);
    Ok(builder)
}

/// Relative paths of every file under a root, plus fuzzy search over them.
#[derive(Debug, Default, Clone)]
pub struct PathIndex {
    root: PathBuf,
    paths: Vec<String>,
    pub scanned_in_ms: u128,
}

impl PathIndex {
    /// Walks `root` (parallel) and collects file paths relative to it.
    ///
    /// # Errors
    ///
    /// Invalid exclusion globs.
    pub fn scan(root: &Path, extra_excludes: &[String]) -> Result<Self, ignore::Error> {
        let started = Instant::now();
        let (tx, rx) = mpsc::channel::<String>();
        walker(root, extra_excludes)?.build_parallel().run(|| {
            let tx = tx.clone();
            let root = root.to_path_buf();
            Box::new(move |entry| {
                if let Ok(entry) = entry
                    && entry.file_type().is_some_and(|kind| kind.is_file())
                    && let Ok(relative) = entry.path().strip_prefix(&root)
                {
                    let _ = tx.send(relative.to_string_lossy().into_owned());
                }
                ignore::WalkState::Continue
            })
        });
        drop(tx);
        let mut paths: Vec<String> = rx.into_iter().collect();
        paths.sort_unstable();
        Ok(Self {
            root: root.to_path_buf(),
            paths,
            scanned_in_ms: started.elapsed().as_millis(),
        })
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.paths.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }

    #[must_use]
    pub fn paths(&self) -> &[String] {
        &self.paths
    }

    /// Best `limit` matches for `query` (nucleo scoring, smart case); an
    /// empty query lists the first paths in order.
    #[must_use]
    pub fn fuzzy(&self, query: &str, limit: usize) -> Vec<FuzzyMatch<'_>> {
        if query.trim().is_empty() {
            return self
                .paths
                .iter()
                .take(limit)
                .map(|path| FuzzyMatch {
                    path,
                    score: 0,
                    indices: Vec::new(),
                })
                .collect();
        }
        let mut matcher = Matcher::new(Config::DEFAULT.match_paths());
        let pattern = Pattern::parse(query, CaseMatching::Smart, Normalization::Smart);
        let mut buffer = Vec::new();
        let mut scored: Vec<(u32, &String)> = self
            .paths
            .iter()
            .filter_map(|path| {
                pattern
                    .score(Utf32Str::new(path, &mut buffer), &mut matcher)
                    .map(|score| (score, path))
            })
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.len().cmp(&b.1.len())));
        scored
            .into_iter()
            .take(limit)
            .map(|(score, path)| {
                let mut indices = Vec::new();
                pattern.indices(Utf32Str::new(path, &mut buffer), &mut matcher, &mut indices);
                indices.sort_unstable();
                indices.dedup();
                FuzzyMatch {
                    path,
                    score,
                    indices,
                }
            })
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FuzzyMatch<'a> {
    pub path: &'a str,
    pub score: u32,
    /// Char indices of the matched characters, for highlighting.
    pub indices: Vec<u32>,
}

/// A change to a watched file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileChange {
    Modified(PathBuf),
    Removed(PathBuf),
}

/// Watches individual files (the open buffers) and reports changes
/// through a channel the UI polls.
pub struct FileWatcher {
    watcher: notify::RecommendedWatcher,
    events: Receiver<FileChange>,
    watched: Vec<PathBuf>,
}

impl FileWatcher {
    /// # Errors
    ///
    /// The platform watcher failing to initialise (inotify limits).
    pub fn new() -> notify::Result<Self> {
        let (tx, events) = mpsc::channel();
        let watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
            let Ok(event) = event else { return };
            let change = match event.kind {
                notify::EventKind::Modify(_) | notify::EventKind::Create(_) => FileChange::Modified,
                notify::EventKind::Remove(_) => FileChange::Removed,
                _ => return,
            };
            for path in event.paths {
                let _ = tx.send(change(path));
            }
        })?;
        Ok(Self {
            watcher,
            events,
            watched: Vec::new(),
        })
    }

    /// Starts watching `path` (idempotent).
    ///
    /// # Errors
    ///
    /// The platform refusing the watch.
    pub fn watch(&mut self, path: &Path) -> notify::Result<()> {
        use notify::Watcher as _;
        if self.watched.iter().any(|watched| watched == path) {
            return Ok(());
        }
        self.watcher
            .watch(path, notify::RecursiveMode::NonRecursive)?;
        self.watched.push(path.to_path_buf());
        Ok(())
    }

    pub fn unwatch(&mut self, path: &Path) {
        use notify::Watcher as _;
        if let Some(index) = self.watched.iter().position(|watched| watched == path) {
            self.watched.remove(index);
            let _ = self.watcher.unwatch(path);
        }
    }

    /// Changes since the last poll, deduplicated.
    pub fn poll(&mut self) -> Vec<FileChange> {
        let mut changes: Vec<FileChange> = self.events.try_iter().collect();
        changes.dedup();
        changes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("forge-project-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src/deep")).unwrap();
        std::fs::create_dir_all(dir.join("node_modules/pkg")).unwrap();
        std::fs::create_dir_all(dir.join("target")).unwrap();
        std::fs::write(dir.join("src/main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(dir.join("src/deep/config_loader.rs"), "").unwrap();
        std::fs::write(dir.join("README.md"), "# hi\n").unwrap();
        std::fs::write(dir.join("node_modules/pkg/index.js"), "").unwrap();
        std::fs::write(dir.join("target/out.bin"), "").unwrap();
        std::fs::write(dir.join("secret.log"), "").unwrap();
        std::fs::write(dir.join(".gitignore"), "*.log\n").unwrap();
        dir
    }

    #[test]
    fn index_skips_global_excludes_and_gitignored_files() {
        let dir = fixture("index");
        let index = PathIndex::scan(&dir, &[]).unwrap();
        assert_eq!(
            index.paths(),
            ["README.md", "src/deep/config_loader.rs", "src/main.rs"]
        );
        let found = index.fuzzy("cfgl", 10);
        assert_eq!(found[0].path, "src/deep/config_loader.rs");
        assert!(!found[0].indices.is_empty());
        assert_eq!(index.fuzzy("", 2).len(), 2);
        assert!(index.fuzzy("zzzz", 10).is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn watcher_reports_modifications_of_watched_files() {
        let dir = fixture("watch");
        let file = dir.join("README.md");
        let mut watcher = FileWatcher::new().unwrap();
        watcher.watch(&file).unwrap();
        watcher.watch(&file).unwrap();
        std::fs::write(&file, "# changed\n").unwrap();
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        let mut changes = Vec::new();
        while changes.is_empty() && Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(50));
            changes = watcher.poll();
        }
        assert!(
            changes
                .iter()
                .any(|change| matches!(change, FileChange::Modified(path) if path == &file)),
            "{changes:?}"
        );
        watcher.unwatch(&file);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
