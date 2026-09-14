//! Workspace features of the editor: the fuzzy file finder, project-wide
//! search, the in-buffer find/replace bar and the watcher that reloads
//! buffers changed on disk.

use crate::{
    editor::EditorTab,
    ipc::UiEvent,
    window::{ForgeWindow, NotificationLevel},
};
use forge_buffer::{Edit, Selection, Selections};
use forge_gui::i18n::{tr, trf};
use forge_project::{FileChange, FileWatcher, PathIndex};
use forge_search::{ProjectMatch, ProjectSearch, SearchEvent, SearchOptions};
use gpui::Context;
use proto_ipc::KeyMods;
use std::{
    ops::Range,
    path::Path,
    time::{Duration, Instant},
};

/// Rows shown by the list overlays.
pub const OVERLAY_ROWS: usize = 12;
/// Project search stops after this many matches.
const PROJECT_SEARCH_LIMIT: usize = 1000;
/// The path index is rescanned when the finder opens after this long.
const INDEX_TTL: Duration = Duration::from_secs(30);
const FIND_LIMIT: usize = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinderMode {
    /// Fuzzy file finder over the path index.
    Files,
    /// Regex/literal search across the workspace.
    ProjectSearch,
}

/// A list overlay with a query line: the file finder or project search.
pub struct Finder {
    pub mode: FinderMode,
    pub query: String,
    pub index: usize,
    pub options: SearchOptions,
    pub error: Option<String>,
}

/// One row of a finder list, already formatted for the chrome.
pub struct FinderRow {
    pub label: String,
    pub detail: String,
    /// Char indices of `label` to emphasise (fuzzy hits).
    pub emphasis: Vec<u32>,
}

/// Workspace state shared by the finder modes and the watcher.
#[derive(Default)]
pub struct ProjectState {
    pub index: Option<PathIndex>,
    pub indexing: bool,
    indexed_at: Option<Instant>,
    search: Option<ProjectSearch>,
    pub results: Vec<ProjectMatch>,
    /// `(truncated, files)` once the search finished.
    pub done: Option<(bool, usize)>,
    pub watcher: Option<FileWatcher>,
}

/// Find/replace inside the active editor.
#[derive(Default)]
pub struct EditorFind {
    pub open: bool,
    pub query: String,
    pub replacement: String,
    /// The replacement field has focus.
    pub replacing: bool,
    pub options: SearchOptions,
    pub matches: Vec<Range<usize>>,
    pub current: Option<usize>,
    pub error: Option<String>,
    /// Buffer version the matches were computed for.
    version: Option<u64>,
}

impl EditorFind {
    pub fn label(&self) -> String {
        match (self.current, self.matches.len()) {
            (_, 0) if self.query.is_empty() => String::new(),
            (_, 0) => tr("no matches").into(),
            (Some(index), count) => format!("{}/{count}", index + 1),
            (None, count) => format!("{count}"),
        }
    }
}

impl ForgeWindow {
    // ----- file finder and project search --------------------------------

    pub fn open_finder(&mut self, mode: FinderMode, cx: &mut Context<Self>) {
        self.palette.open = false;
        self.finder = Some(Finder {
            mode,
            query: String::new(),
            index: 0,
            options: SearchOptions::default(),
            error: None,
        });
        if mode == FinderMode::Files {
            self.ensure_index();
        }
        self.project.results.clear();
        self.project.done = None;
        self.project.search = None;
        cx.notify();
    }

    /// Scans the workspace on a thread unless a fresh index exists.
    fn ensure_index(&mut self) {
        let fresh = self
            .project
            .indexed_at
            .is_some_and(|at| at.elapsed() < INDEX_TTL);
        if self.project.indexing || (fresh && self.project.index.is_some()) {
            return;
        }
        self.project.indexing = true;
        let root = self.factory.cwd.clone();
        let events = self.event_tx.clone();
        std::thread::Builder::new()
            .name("forge-index".into())
            .spawn(move || {
                let index = PathIndex::scan(&root, &[]);
                let _ = events.send(UiEvent::IndexReady {
                    index: Box::new(index.map_err(|error| error.to_string())),
                });
            })
            .expect("spawn index scan");
    }

    pub fn on_index_ready(&mut self, index: Result<PathIndex, String>, cx: &mut Context<Self>) {
        self.project.indexing = false;
        match index {
            Ok(index) => {
                tracing::debug!(files = index.len(), ms = index.scanned_in_ms, "path index");
                self.project.index = Some(index);
                self.project.indexed_at = Some(Instant::now());
            }
            Err(error) => self.notify_user(NotificationLevel::Error, format!("Índice: {error}")),
        }
        cx.notify();
    }

    /// Rows for the open finder, computed on demand by the chrome.
    pub fn finder_rows(&self) -> Vec<FinderRow> {
        let Some(finder) = &self.finder else {
            return Vec::new();
        };
        match finder.mode {
            FinderMode::Files => self.project.index.as_ref().map_or_else(Vec::new, |index| {
                index
                    .fuzzy(&finder.query, OVERLAY_ROWS)
                    .into_iter()
                    .map(|found| FinderRow {
                        label: found.path.to_owned(),
                        detail: String::new(),
                        emphasis: found.indices,
                    })
                    .collect()
            }),
            FinderMode::ProjectSearch => {
                let first = finder.index.saturating_sub(OVERLAY_ROWS - 1);
                self.project
                    .results
                    .iter()
                    .skip(first)
                    .take(OVERLAY_ROWS)
                    .map(|found| FinderRow {
                        label: format!("{}:{}", found.path.display(), found.line),
                        detail: found.text.trim().chars().take(120).collect(),
                        emphasis: Vec::new(),
                    })
                    .collect()
            }
        }
    }

    /// Index of the first row `finder_rows` returned, for selection maths.
    pub fn finder_first_row(&self) -> usize {
        match self.finder.as_ref().map(|finder| finder.mode) {
            Some(FinderMode::ProjectSearch) => self
                .finder
                .as_ref()
                .map_or(0, |finder| finder.index.saturating_sub(OVERLAY_ROWS - 1)),
            _ => 0,
        }
    }

    pub fn finder_status(&self) -> String {
        let Some(finder) = &self.finder else {
            return String::new();
        };
        if let Some(error) = &finder.error {
            return error.clone();
        }
        match finder.mode {
            FinderMode::Files => match &self.project.index {
                Some(index) => trf("{} files", &[&index.len()]),
                None => tr("indexing…").into(),
            },
            FinderMode::ProjectSearch => match self.project.done {
                Some((truncated, files)) => trf(
                    "{}{} results · {} files",
                    &[
                        &self.project.results.len(),
                        &if truncated { "+" } else { "" },
                        &files,
                    ],
                ),
                None if finder.query.is_empty() => tr("type to search the project").into(),
                None => trf("{} results…", &[&self.project.results.len()]),
            },
        }
    }

    pub fn finder_key(
        &mut self,
        key: &str,
        key_char: Option<&str>,
        modifiers: KeyMods,
        cx: &mut Context<Self>,
    ) {
        let Some(finder) = &mut self.finder else {
            return;
        };
        let plain = !modifiers.control && !modifiers.alt;
        match key {
            "escape" => {
                self.finder = None;
                self.project.search = None;
            }
            "up" => finder.index = finder.index.saturating_sub(1),
            "down" => finder.index += 1,
            "pageup" => finder.index = finder.index.saturating_sub(OVERLAY_ROWS),
            "pagedown" => finder.index += OVERLAY_ROWS,
            "enter" => return self.finder_accept(cx),
            "backspace" if plain => {
                finder.query.pop();
                self.finder_changed();
            }
            "r" if modifiers.alt => {
                finder.options.regex = !finder.options.regex;
                self.finder_changed();
            }
            "c" if modifiers.alt => {
                finder.options.case_sensitive = !finder.options.case_sensitive;
                self.finder_changed();
            }
            "w" if modifiers.alt => {
                finder.options.whole_word = !finder.options.whole_word;
                self.finder_changed();
            }
            _ if plain => {
                if let Some(text) = key_char {
                    finder.query.push_str(text);
                    self.finder_changed();
                }
            }
            _ => {}
        }
        self.clamp_finder_index();
        cx.notify();
    }

    fn clamp_finder_index(&mut self) {
        let count = match self.finder.as_ref().map(|finder| finder.mode) {
            Some(FinderMode::Files) => self.finder_rows().len(),
            Some(FinderMode::ProjectSearch) => self.project.results.len(),
            None => 0,
        };
        if let Some(finder) = &mut self.finder {
            finder.index = finder.index.min(count.saturating_sub(1));
        }
    }

    /// The query changed: restart the project search (the file finder
    /// filters lazily when rendering).
    fn finder_changed(&mut self) {
        let Some(finder) = &mut self.finder else {
            return;
        };
        finder.index = 0;
        finder.error = None;
        if finder.mode != FinderMode::ProjectSearch {
            return;
        }
        self.project.results.clear();
        self.project.done = None;
        self.project.search = None;
        if finder.query.is_empty() {
            return;
        }
        match forge_search::search_project(
            &self.factory.cwd,
            &finder.query,
            finder.options,
            PROJECT_SEARCH_LIMIT,
        ) {
            Ok(search) => self.project.search = Some(search),
            Err(error) => finder.error = Some(error),
        }
    }

    /// Drains streamed search results; returns whether anything arrived.
    pub fn poll_project_search(&mut self) -> bool {
        let Some(search) = &self.project.search else {
            return false;
        };
        let mut changed = false;
        let mut finished = false;
        for event in search.events.try_iter() {
            changed = true;
            match event {
                SearchEvent::Match(found) => self.project.results.push(found),
                SearchEvent::Done { truncated, files } => {
                    self.project.done = Some((truncated, files));
                    finished = true;
                }
                SearchEvent::Error(error) => {
                    if let Some(finder) = &mut self.finder {
                        finder.error = Some(error);
                    }
                }
            }
        }
        if finished {
            self.project.search = None;
        }
        changed
    }

    fn finder_accept(&mut self, cx: &mut Context<Self>) {
        let Some(finder) = self.finder.take() else {
            return;
        };
        self.project.search = None;
        match finder.mode {
            FinderMode::Files => {
                let path = self.project.index.as_ref().and_then(|index| {
                    index
                        .fuzzy(&finder.query, OVERLAY_ROWS)
                        .get(finder.index)
                        .map(|found| index.root().join(found.path))
                });
                if let Some(path) = path {
                    self.open_file(&path, None, None, cx);
                }
            }
            FinderMode::ProjectSearch => {
                if let Some(found) = self.project.results.get(finder.index).cloned() {
                    let path = self.factory.cwd.join(&found.path);
                    let column = u32::try_from(found.range.start).ok().map(|c| c + 1);
                    self.open_file(&path, u32::try_from(found.line).ok(), column, cx);
                }
            }
        }
        cx.notify();
    }

    // ----- in-buffer find/replace -----------------------------------------

    pub fn open_find(&mut self, replace: bool, cx: &mut Context<Self>) {
        if self.active_tab().editor().is_none_or(EditorTab::is_large) {
            return;
        }
        // Seed the query with the selection, like most editors.
        let seed = self.active_tab().editor().and_then(|editor| {
            let primary = editor.buffer.selections().primary();
            (!primary.is_empty() && !editor.buffer.slice(primary.range()).contains('\n'))
                .then(|| editor.buffer.slice(primary.range()))
        });
        if let Some(seed) = seed {
            self.find.query = seed;
        }
        self.find.open = true;
        self.find.replacing = replace;
        self.find.version = None;
        self.refresh_find();
        cx.notify();
    }

    pub fn close_find(&mut self, cx: &mut Context<Self>) {
        self.find.open = false;
        self.find.matches.clear();
        self.find.current = None;
        cx.notify();
    }

    /// Recomputes matches when the query or the buffer changed.
    pub fn refresh_find(&mut self) {
        if !self.find.open {
            return;
        }
        let Some((version, rope, head)) = self.active_tab().editor().map(|editor| {
            (
                editor.buffer.version(),
                editor.buffer.rope(),
                editor.buffer.selections().primary().head,
            )
        }) else {
            return;
        };
        if self.find.version == Some(version) {
            return;
        }
        self.find.version = Some(version);
        self.find.error = None;
        if self.find.query.is_empty() {
            self.find.matches.clear();
            self.find.current = None;
            return;
        }
        match forge_search::compile(&self.find.query, self.find.options) {
            Ok(pattern) => {
                self.find.matches = forge_search::search_rope(&rope, &pattern, FIND_LIMIT);
                self.find.current = if self.find.matches.is_empty() {
                    None
                } else {
                    // The current match is the one at/after the cursor.
                    let at = self
                        .find
                        .matches
                        .partition_point(|range| range.start < head);
                    Some(at.min(self.find.matches.len() - 1))
                };
            }
            Err(error) => {
                self.find.error = Some(error);
                self.find.matches.clear();
                self.find.current = None;
            }
        }
    }

    /// Selects the current match in the editor and reveals it.
    fn find_reveal(&mut self, cx: &mut Context<Self>) {
        let Some(range) = self
            .find
            .current
            .and_then(|index| self.find.matches.get(index).cloned())
        else {
            return;
        };
        if let Some(editor) = self.active_tab_mut().editor_mut() {
            editor
                .buffer
                .set_selections(Selections::single(Selection::new(range.start, range.end)));
            editor.goal_column = None;
            editor.follow_cursor();
        }
        cx.notify();
    }

    fn find_step(&mut self, forward: bool, cx: &mut Context<Self>) {
        let count = self.find.matches.len();
        if count == 0 {
            return;
        }
        // Enter on a freshly computed list selects the match at the cursor
        // first; later presses move on.
        let selected_now = self.active_tab().editor().is_some_and(|editor| {
            let primary = editor.buffer.selections().primary();
            self.find
                .current
                .and_then(|index| self.find.matches.get(index))
                .is_some_and(|range| primary.range() == *range)
        });
        self.find.current = Some(match (self.find.current, forward, selected_now) {
            (Some(index), _, false) => index,
            (Some(index), true, true) => (index + 1) % count,
            (Some(index), false, true) => (index + count - 1) % count,
            (None, true, _) => 0,
            (None, false, _) => count - 1,
        });
        self.find_reveal(cx);
    }

    /// Replaces the current match (or every match) with the replacement.
    fn find_replace(&mut self, all: bool, cx: &mut Context<Self>) {
        let replacement = self.find.replacement.clone();
        let edits: Vec<Edit> = if all {
            self.find
                .matches
                .iter()
                .map(|range| Edit {
                    range: range.clone(),
                    text: replacement.clone(),
                })
                .collect()
        } else {
            self.find
                .current
                .and_then(|index| self.find.matches.get(index).cloned())
                .map(|range| Edit {
                    range,
                    text: replacement.clone(),
                })
                .into_iter()
                .collect()
        };
        if edits.is_empty() {
            return;
        }
        let count = edits.len();
        if let Some(editor) = self.active_tab_mut().editor_mut() {
            if let Err(error) = editor.buffer.edit(edits, false) {
                self.notify_user(
                    NotificationLevel::Error,
                    format!("Reemplazo fallido: {error}"),
                );
                return;
            }
            editor.follow_cursor();
            editor.sync_syntax();
        }
        self.refresh_find();
        if all {
            self.notify_user(NotificationLevel::Info, format!("{count} reemplazos"));
        } else {
            self.find_step(true, cx);
        }
        cx.notify();
    }

    pub fn find_key(
        &mut self,
        key: &str,
        key_char: Option<&str>,
        modifiers: KeyMods,
        cx: &mut Context<Self>,
    ) {
        let plain = !modifiers.control && !modifiers.alt;
        match key {
            "escape" => return self.close_find(cx),
            "enter" if modifiers.control && modifiers.alt => return self.find_replace(true, cx),
            "enter" if modifiers.control => return self.find_replace(false, cx),
            "enter" => return self.find_step(!modifiers.shift, cx),
            "tab" => self.find.replacing = !self.find.replacing,
            "backspace" if plain => {
                if self.find.replacing {
                    self.find.replacement.pop();
                } else {
                    self.find.query.pop();
                    self.find.version = None;
                }
            }
            "r" if modifiers.alt => {
                self.find.options.regex = !self.find.options.regex;
                self.find.version = None;
            }
            "c" if modifiers.alt => {
                self.find.options.case_sensitive = !self.find.options.case_sensitive;
                self.find.version = None;
            }
            "w" if modifiers.alt => {
                self.find.options.whole_word = !self.find.options.whole_word;
                self.find.version = None;
            }
            _ if plain => {
                if let Some(text) = key_char {
                    if self.find.replacing {
                        self.find.replacement.push_str(text);
                    } else {
                        self.find.query.push_str(text);
                        self.find.version = None;
                    }
                }
            }
            _ => {}
        }
        self.refresh_find();
        cx.notify();
    }

    // ----- watcher ----------------------------------------------------------

    pub fn watch_file(&mut self, path: &Path) {
        if self.project.watcher.is_none() {
            match FileWatcher::new() {
                Ok(watcher) => self.project.watcher = Some(watcher),
                Err(error) => {
                    tracing::warn!(%error, "file watcher unavailable");
                    return;
                }
            }
        }
        if let Some(watcher) = &mut self.project.watcher
            && let Err(error) = watcher.watch(path)
        {
            tracing::warn!(%error, path = %path.display(), "watch failed");
        }
    }

    pub fn unwatch_file(&mut self, path: &Path) {
        if let Some(watcher) = &mut self.project.watcher {
            watcher.unwatch(path);
        }
    }

    /// Applies disk changes to open editors: clean buffers reload, dirty
    /// ones warn. Returns whether anything visible changed.
    pub fn poll_watcher(&mut self, cx: &mut Context<Self>) -> bool {
        let changes = self
            .project
            .watcher
            .as_mut()
            .map(FileWatcher::poll)
            .unwrap_or_default();
        let mut dirty = false;
        for change in changes {
            let (path, removed) = match change {
                FileChange::Modified(path) => (path, false),
                FileChange::Removed(path) => (path, true),
            };
            let Some(index) = self.tabs.iter().position(|tab| {
                tab.editor()
                    .is_some_and(|editor| editor.path() == Some(path.as_path()))
            }) else {
                continue;
            };
            let name = path.display().to_string();
            let Some(editor) = self.tabs[index].editor_mut() else {
                continue;
            };
            if removed {
                self.notify_user(
                    NotificationLevel::Warning,
                    trf("{} was deleted on disk; save to recreate it", &[&name]),
                );
            } else if editor.is_large() {
                self.notify_user(
                    NotificationLevel::Warning,
                    trf(
                        "{} changed on disk; reopen it to see the new content",
                        &[&name],
                    ),
                );
            } else if editor.buffer.is_dirty() {
                self.notify_user(
                    NotificationLevel::Warning,
                    trf("{} changed on disk and has unsaved changes", &[&name]),
                );
            } else if editor.reload_from_disk() {
                self.notify_user(NotificationLevel::Info, format!("{name} recargado"));
            }
            dirty = true;
        }
        if dirty {
            cx.notify();
        }
        dirty
    }
}
