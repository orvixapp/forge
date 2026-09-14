//! Editor tabs: a [`forge_buffer::Buffer`] painted line by line with GPUI's
//! text shaper, plus the keyboard and mouse handling that turns keystrokes
//! into transactions. Terminal tabs keep their own element in
//! `grid_element.rs`; both share the pane tree.

use crate::{
    grid_element::{CellMetrics, color},
    ipc::{GitDiffResult, UiEvent},
    window::{ForgeWindow, NotificationLevel, Tab, TabContent},
};
use forge_buffer::{
    Buffer, BufferError, Cursor, Edit, Journal, LargeFile, LoadedFile, Motion, Position, Selection,
    Selections, Transaction, large,
};
use forge_gui::i18n::{tr, trf};
use forge_syntax::{Span, SyntaxState, Token};
use gpui::{
    App, Bounds, ClipboardItem, ContentMask, Context, Element, ElementId, Entity, Font,
    GlobalElementId, Hsla, InspectorElementId, IntoElement, LayoutId, MouseDownEvent, Pixels,
    Point, SharedString, Style, TextRun, Window, fill, point, px, size,
};
use proto_ipc::KeyMods;
use std::{
    borrow::Cow,
    ops::Range,
    path::{Path, PathBuf},
    sync::mpsc::Sender,
    time::{Duration, Instant},
};

/// Helix-like editing modes, active when `editor.modal` is set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditorMode {
    Insert,
    Normal,
}

/// Space between the gutter text and the code, and around the gutter.
const GUTTER_PADDING: f32 = 12.0;

#[allow(clippy::struct_excessive_bools)]
pub struct EditorTab {
    pub buffer: Buffer,
    /// `None` for an untitled buffer.
    pub file: Option<LoadedFile>,
    /// Column kept across vertical motions of the primary cursor.
    pub goal_column: Option<usize>,
    /// First visible line.
    pub scroll_line: usize,
    /// Rows that fit in the last paint; drives page motions and follow.
    pub visible_rows: usize,
    /// Where the text (not the gutter) was last painted, window space.
    last_text_bounds: Option<Bounds<Pixels>>,
    drag_anchor: Option<usize>,
    /// Edits recovered from a journal after a crash; cleared on save.
    pub recovered: bool,
    /// Line to centre on the next paint, once the row count is known.
    pending_center: Option<usize>,
    /// tree-sitter state; `None` for unknown languages and large files.
    pub syntax: Option<SyntaxState>,
    /// Large-file mode: the text lives in this map, read-only, until the
    /// user materialises it (§13.4).
    pub large: Option<LargeFile>,
    pub mode: EditorMode,
    /// Normal-mode motions extend the selection instead of moving it.
    pub extending: bool,
    /// When the buffer last changed, for autosave.
    pub last_edit: Instant,
    /// `Alt+Z` toggles wrapping per editor; `None` follows the config.
    pub word_wrap: Option<bool>,
    /// Columns per visual row when wrapping, from the last paint.
    wrap_columns: Option<usize>,
    /// Tab width used by the last paint, for row maths outside painting.
    tab_size: usize,
    /// Horizontal scroll in pixels (no-wrap mode).
    pub scroll_x: f32,
    /// Bring the cursor's column into view on the next paint.
    reveal_x: bool,
    /// Minimap geometry from the last paint: bounds and its first line.
    minimap_bounds: Option<Bounds<Pixels>>,
    minimap_top: usize,
    /// The mouse is dragging the minimap slider.
    pub dragging_minimap: bool,
    /// Coarse highlights for the minimap, refreshed at most twice a second.
    minimap_cache: Option<MinimapCache>,
    /// Slider drag: where it started and the scroll line at that moment.
    minimap_drag: Option<(Pixels, usize)>,
    /// Git state: repository/branch, `HEAD` text and the gutter diff.
    pub git: GitState,
}

/// What the editor knows about its file in git.
#[derive(Default)]
pub struct GitState {
    pub info: Option<forge_git::RepoInfo>,
    /// `HEAD` content, cached until save or reload; `None` = untracked.
    head: Option<String>,
    pub diff: forge_git::LineDiff,
    /// Buffer version the diff describes.
    pub diff_version: Option<u64>,
    /// A diff is being computed for this version.
    pending: Option<u64>,
    /// The repository was looked up at least once (so `info == None`
    /// means "not in a repository", not "unknown yet").
    discovered: bool,
}

/// Edits settle this long before the gutter diff is recomputed.
const GIT_DIFF_DEBOUNCE: Duration = Duration::from_millis(300);
/// Width of the change bar between the line numbers and the text.
const GIT_GUTTER_WIDTH: f32 = 3.0;

struct MinimapCache {
    at: Instant,
    version: u64,
    lines: Range<usize>,
    spans: Vec<Vec<Span>>,
}

/// One painted row: columns `cols` (chars of the line) of buffer `line`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct VisualRow {
    line: usize,
    cols: Range<usize>,
    /// First row of its line (gets the line number).
    first: bool,
    /// Last row of its line (owns the newline and the end-of-line cursor).
    last: bool,
    /// Display column the row starts at (continuations keep the indent).
    start_column: usize,
}

/// Width of the minimap strip, and the smallest text area that keeps it.
const MINIMAP_WIDTH: f32 = 90.0;
const MINIMAP_MIN_TEXT_WIDTH: f32 = 320.0;
/// Pixels per line in the minimap (1 px of ink, 1 px of gap).
const MINIMAP_ROW: f32 = 2.0;
const MINIMAP_REFRESH: Duration = Duration::from_millis(1000);
const MINIMAP_MARGIN_LINES: usize = 40;

/// Bracket and quote pairs closed automatically while typing.
const AUTO_PAIRS: &[(char, char)] = &[('(', ')'), ('[', ']'), ('{', '}'), ('"', '"'), ('\'', '\'')];

/// Line-comment prefix per tree-sitter language.
fn line_comment(language: Option<&str>) -> Option<&'static str> {
    match language? {
        "rust" | "c" | "javascript" => Some("//"),
        "python" | "bash" | "toml" => Some("#"),
        _ => None,
    }
}

/// Longest line prefix painted in large mode.
const LARGE_LINE_PAINT_BYTES: usize = 4096;
/// Files above this size skip tree-sitter even after materialising.
const SYNTAX_MAX_BYTES: usize = 20 * 1024 * 1024;

impl EditorTab {
    pub fn new(buffer: Buffer, file: Option<LoadedFile>) -> Self {
        Self {
            buffer,
            file,
            goal_column: None,
            scroll_line: 0,
            visible_rows: 1,
            last_text_bounds: None,
            drag_anchor: None,
            recovered: false,
            pending_center: None,
            syntax: None,
            large: None,
            mode: EditorMode::Insert,
            extending: false,
            last_edit: Instant::now(),
            word_wrap: None,
            wrap_columns: None,
            tab_size: 4,
            scroll_x: 0.0,
            reveal_x: false,
            minimap_bounds: None,
            minimap_top: 0,
            dragging_minimap: false,
            minimap_cache: None,
            minimap_drag: None,
            git: GitState::default(),
        }
    }

    /// Starts a slider drag at `y` (window space).
    pub fn begin_minimap_drag(&mut self, y: Pixels) {
        self.dragging_minimap = true;
        self.minimap_drag = Some((y, self.scroll_line));
    }

    /// Dragging the slider moves through the whole document over the
    /// strip's height, like VS Code: a short drag covers many lines in a
    /// long file.
    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
    pub fn drag_minimap(&mut self, y: Pixels) {
        let Some((start_y, start_line)) = self.minimap_drag else {
            return self.scroll_to_minimap(y);
        };
        let Some(bounds) = self.minimap_bounds else {
            return;
        };
        let total = self.total_lines();
        let capacity = (f32::from(bounds.size.height) / MINIMAP_ROW).max(1.0);
        let lines_per_px = if (total as f32) > capacity {
            total as f32 / f32::from(bounds.size.height)
        } else {
            1.0 / MINIMAP_ROW
        };
        let delta = f32::from(y - start_y) * lines_per_px;
        let target = (start_line as f32 + delta).round() as i64;
        self.scroll_line = usize::try_from(target.max(0))
            .unwrap_or(0)
            .min(total.saturating_sub(1));
    }

    /// Kicks off a `HEAD` diff on a thread when the buffer settled; the
    /// result comes back through `UiEvent::GitDiff`.
    pub fn refresh_git(&mut self, tab_id: u64, events: &Sender<UiEvent>) {
        if self.large.is_some() {
            return;
        }
        let Some(path) = self.path().map(Path::to_path_buf) else {
            return;
        };
        let version = self.buffer.version();
        if self.git.diff_version == Some(version)
            || self.git.pending == Some(version)
            || self.last_edit.elapsed() < GIT_DIFF_DEBOUNCE
        {
            return;
        }
        if self.git.discovered && self.git.info.is_none() {
            return;
        }
        self.git.pending = Some(version);
        let text = self.buffer.text();
        let info = self.git.info.clone();
        let head = self.git.head.clone();
        let discovered = self.git.discovered;
        let events = events.clone();
        std::thread::Builder::new()
            .name("forge-git-diff".into())
            .spawn(move || {
                let info = if discovered {
                    info
                } else {
                    forge_git::discover(&path)
                };
                let head = match (&info, head) {
                    (None, _) => None,
                    (Some(_), Some(head)) => Some(head),
                    (Some(info), None) => forge_git::head_text(info).ok().flatten(),
                };
                let diff = match &info {
                    Some(_) => forge_git::line_diff(head.as_deref(), &text),
                    None => forge_git::LineDiff::default(),
                };
                let _ = events.send(UiEvent::GitDiff {
                    tab_id,
                    version,
                    state: Box::new(GitDiffResult { info, head, diff }),
                });
            })
            .expect("spawn git diff");
    }

    /// Takes a finished diff; stale ones (older version) are dropped.
    pub fn git_ready(&mut self, version: u64, result: GitDiffResult) -> bool {
        self.git.pending = None;
        self.git.discovered = true;
        self.git.info = result.info;
        self.git.head = result.head;
        if self
            .git
            .diff_version
            .is_some_and(|current| current > version)
        {
            return false;
        }
        self.git.diff = result.diff;
        self.git.diff_version = Some(version);
        true
    }

    /// The file on disk changed (save/reload): `HEAD` may differ now.
    pub fn invalidate_git(&mut self) {
        self.git.head = None;
        self.git.diff_version = None;
        self.git.discovered = false;
    }

    /// Whether a window position is over the minimap strip.
    pub fn on_minimap(&self, position: Point<Pixels>) -> bool {
        self.minimap_bounds
            .is_some_and(|bounds| bounds.contains(&position))
    }

    /// Scrolls so the minimap row under `y` sits in the middle of the view.
    pub fn scroll_to_minimap(&mut self, y: Pixels) {
        let Some(bounds) = self.minimap_bounds else {
            return;
        };
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let row = (f32::from(y - bounds.origin.y) / MINIMAP_ROW).max(0.0) as usize;
        let line = self.minimap_top + row;
        self.scroll_line = line
            .saturating_sub(self.visible_rows / 2)
            .min(self.total_lines().saturating_sub(1));
    }

    /// Visual rows starting at buffer line `first_line`, at most `max_rows`.
    fn visual_rows(&self, first_line: usize, max_rows: usize, tab_size: usize) -> Vec<VisualRow> {
        let mut rows = Vec::with_capacity(max_rows);
        let total = self.total_lines();
        let mut line = first_line;
        while rows.len() < max_rows && line < total {
            let text = self.buffer.line(line).unwrap_or_default();
            match self.wrap_columns {
                Some(columns) => {
                    let indent = text
                        .chars()
                        .take_while(|c| *c == ' ' || *c == '\t')
                        .count()
                        .min(columns / 2);
                    let pieces = wrap_line(&text, columns, indent, tab_size);
                    let count = pieces.len();
                    for (index, cols) in pieces.into_iter().enumerate() {
                        if rows.len() == max_rows {
                            break;
                        }
                        rows.push(VisualRow {
                            line,
                            cols,
                            first: index == 0,
                            last: index + 1 == count,
                            start_column: if index == 0 { 0 } else { indent },
                        });
                    }
                }
                None => rows.push(VisualRow {
                    line,
                    cols: 0..text.chars().count(),
                    first: true,
                    last: true,
                    start_column: 0,
                }),
            }
            line += 1;
        }
        rows
    }

    /// Visual rows a buffer line occupies with the current wrap width.
    fn rows_of_line(&self, line: usize, tab_size: usize) -> usize {
        match self.wrap_columns {
            Some(columns) => {
                let text = self.buffer.line(line).unwrap_or_default();
                let indent = text
                    .chars()
                    .take_while(|c| *c == ' ' || *c == '\t')
                    .count()
                    .min(columns / 2);
                wrap_line(&text, columns, indent, tab_size).len()
            }
            None => 1,
        }
    }

    /// Opens `path` in large mode (memory-mapped, read-only).
    pub fn large(path: PathBuf, file: LargeFile) -> Self {
        let mut tab = Self::new(Buffer::new(""), None);
        tab.file = Some(LoadedFile {
            path,
            text: String::new(),
            line_ending: forge_buffer::LineEnding::Lf,
            encoding: "UTF-8",
            had_bom: false,
            lossy: false,
        });
        tab.large = Some(file);
        tab
    }

    #[must_use]
    pub fn is_large(&self) -> bool {
        self.large.is_some()
    }

    /// Lines the tab can show: the rope's, or the indexed lines of the map.
    fn total_lines(&self) -> usize {
        self.large
            .as_ref()
            .map_or_else(|| self.buffer.len_lines(), LargeFile::len_lines)
    }

    /// Loads the mapped file into a rope so it can be edited. Refused when
    /// it would take more than half of the available memory.
    pub fn materialize(&mut self) -> Result<(), String> {
        let Some(file) = &self.large else {
            return Ok(());
        };
        let needed = u64::try_from(file.len_bytes()).unwrap_or(u64::MAX);
        if let Some(available) = available_memory_bytes()
            && needed.saturating_mul(2) > available
        {
            return Err(trf(
                "the file takes {} MiB and only {} MiB are available; not loading it into memory",
                &[&(needed / (1024 * 1024)), &(available / (1024 * 1024))],
            ));
        }
        let text = file.text();
        self.buffer = Buffer::new(&text);
        if let Some(loaded) = &mut self.file {
            loaded.line_ending = if text.contains("\r\n") {
                forge_buffer::LineEnding::CrLf
            } else {
                forge_buffer::LineEnding::Lf
            };
        }
        self.large = None;
        self.scroll_line = self
            .scroll_line
            .min(self.buffer.len_lines().saturating_sub(1));
        self.set_cursor(self.buffer.char_at(Position {
            line: self.scroll_line,
            column: 0,
        }));
        Ok(())
    }

    /// Picks the grammar for the file and asks its worker for a tree.
    pub fn detect_language(&mut self) {
        if self.large.is_some() || self.buffer.len_bytes() > SYNTAX_MAX_BYTES {
            self.syntax = None;
            return;
        }
        let first_line = self.buffer.line(0).unwrap_or_default();
        let Some(config) = forge_syntax::detect(self.path(), &first_line) else {
            self.syntax = None;
            return;
        };
        let Ok(mut state) = SyntaxState::new(config) else {
            self.syntax = None;
            return;
        };
        state.parse(&self.buffer.rope(), self.buffer.version(), None);
        self.syntax = Some(state);
    }

    /// Adopts trees the syntax worker finished; true when highlights
    /// changed and the pane should repaint.
    pub fn poll_syntax(&mut self) -> bool {
        self.syntax.as_mut().is_some_and(SyntaxState::poll)
    }

    /// Tells the syntax worker about the last change and marks the edit
    /// time for autosave; every mutation ends up here. The UI tree shifts
    /// immediately, the reparse arrives through [`Self::poll_syntax`].
    pub fn sync_syntax(&mut self) {
        self.last_edit = Instant::now();
        let Some(state) = &mut self.syntax else {
            return;
        };
        let version = self.buffer.version();
        if state.version == version {
            return;
        }
        let rope = self.buffer.rope();
        if state.version + 1 == version {
            for change in self.buffer.last_change() {
                state.edited(
                    &forge_syntax::input_edit(
                        &rope,
                        change.new_start_char,
                        &change.inserted,
                        &change.removed,
                    ),
                    version,
                );
            }
        } else {
            state.invalidate();
        }
        state.parse(&rope, version, None);
    }

    pub fn path(&self) -> Option<&Path> {
        self.file.as_ref().map(|file| file.path.as_path())
    }

    pub fn title(&self) -> Cow<'_, str> {
        match self.path().and_then(Path::file_name) {
            Some(name) => name.to_string_lossy(),
            None => Cow::Borrowed(tr("Untitled")),
        }
    }

    pub fn status(&self) -> String {
        let cursor = self
            .buffer
            .position_of(self.buffer.selections().primary().head);
        let mut parts = vec![format!("Ln {}, Col {}", cursor.line + 1, cursor.column + 1)];
        if self.buffer.selections().len() > 1 {
            parts.push(format!("{} cursores", self.buffer.selections().len()));
        }
        if self.mode == EditorMode::Normal {
            parts.insert(
                0,
                if self.extending {
                    "SELECT".into()
                } else {
                    "NORMAL".into()
                },
            );
        }
        if let Some(file) = &self.file {
            parts.push(file.encoding.to_owned());
            parts.push(match file.line_ending {
                forge_buffer::LineEnding::Lf => "LF".into(),
                forge_buffer::LineEnding::CrLf => "CRLF".into(),
            });
        }
        if let Some(info) = &self.git.info {
            let diff = &self.git.diff;
            let label = if diff.added + diff.modified + diff.deleted > 0 {
                format!(
                    "⎇ {} +{} ~{} −{}",
                    info.branch, diff.added, diff.modified, diff.deleted
                )
            } else {
                format!("⎇ {}", info.branch)
            };
            parts.push(label);
        }
        if self.recovered {
            parts.push(tr("recovered from the journal").into());
        }
        if self.buffer.is_dirty() {
            parts.push(tr("● unsaved").into());
        }
        parts.join(" · ")
    }

    /// Whether a window position falls on the text area.
    pub fn contains(&self, position: Point<Pixels>) -> bool {
        self.last_text_bounds
            .is_some_and(|bounds| bounds.contains(&position))
    }

    /// Keeps the primary cursor inside the visible rows (and, without
    /// wrapping, inside the visible columns on the next paint).
    pub fn follow_cursor(&mut self) {
        let line = self
            .buffer
            .position_of(self.buffer.selections().primary().head)
            .line;
        let rows = self.visible_rows.max(1);
        self.reveal_x = true;
        if line < self.scroll_line {
            self.scroll_line = line;
            return;
        }
        if self.wrap_columns.is_none() {
            if line >= self.scroll_line + rows {
                self.scroll_line = line + 1 - rows;
            }
            return;
        }
        // Wrapped: count visual rows from the top until the cursor's line
        // (including all of its rows) fits.
        let tab_size = self.tab_size;
        loop {
            let mut used = 0;
            for candidate in self.scroll_line..=line {
                used += self.rows_of_line(candidate, tab_size);
                if used > rows {
                    break;
                }
            }
            if used <= rows || self.scroll_line >= line {
                break;
            }
            self.scroll_line += 1;
        }
    }

    pub fn scroll_by(&mut self, delta: isize) {
        let max = self.total_lines().saturating_sub(1);
        self.scroll_line = self.scroll_line.saturating_add_signed(delta).min(max);
    }

    fn set_cursor(&mut self, at: usize) {
        self.buffer
            .set_selections(Selections::single(Selection::point(at)));
        self.goal_column = None;
        self.follow_cursor();
    }

    /// Re-reads the file after it changed on disk, keeping cursor and
    /// scroll where they were. Returns whether the text changed.
    pub fn reload_from_disk(&mut self) -> bool {
        let Some(file) = &self.file else {
            return false;
        };
        let Ok(reloaded) = LoadedFile::read(&file.path) else {
            return false;
        };
        if reloaded.text == self.buffer.text() {
            self.file = Some(reloaded);
            return false;
        }
        let len = self.buffer.len_chars();
        let selections = self.buffer.selections().clone();
        let _ = self.buffer.edit_with_selections(
            vec![Edit {
                range: 0..len,
                text: reloaded.text.clone(),
            }],
            selections.clamped(reloaded.text.chars().count()),
            false,
        );
        self.buffer.mark_saved();
        self.file = Some(reloaded);
        self.sync_syntax();
        self.invalidate_git();
        true
    }

    /// Cursor at a 1-based line and column, as links and the CLI give them.
    pub fn go_to(&mut self, line: u32, column: Option<u32>) {
        if self.large.is_some() {
            self.pending_center = Some(usize::try_from(line.saturating_sub(1)).unwrap_or(0));
            return;
        }
        let at = self.buffer.char_at(Position {
            line: usize::try_from(line.saturating_sub(1)).unwrap_or(0),
            column: usize::try_from(column.unwrap_or(1).saturating_sub(1)).unwrap_or(0),
        });
        self.set_cursor(at);
        // Centre the target instead of pinning it to the bottom edge; the
        // row count is only known after the first paint.
        self.pending_center = Some(self.buffer.position_of(at).line);
    }
}

/// Bytes of `text` with tabs expanded, and the display byte offset of every
/// char index (chars + 1 entries), so cursor x positions come from the
/// shaped display text.
fn display_line(text: &str, tab_size: usize) -> (String, Vec<usize>) {
    display_line_from(text, tab_size, 0)
}

/// [`display_line`] for a slice that starts at display column `column`, so
/// tab stops keep their positions on wrapped continuation rows.
fn display_line_from(text: &str, tab_size: usize, column: usize) -> (String, Vec<usize>) {
    let mut display = String::with_capacity(text.len());
    let mut offsets = Vec::with_capacity(text.len() + 1);
    let mut column = column;
    for c in text.chars() {
        offsets.push(display.len());
        if c == '\t' {
            let width = tab_size - column % tab_size;
            display.extend(std::iter::repeat_n(' ', width));
            column += width;
        } else {
            display.push(c);
            column += 1;
        }
    }
    offsets.push(display.len());
    (display, offsets)
}

/// Splits a line into char ranges that fit `columns` display columns,
/// breaking after whitespace when possible (VS Code's `wordWrap`);
/// continuation rows are indented by `indent` columns like the first.
fn wrap_line(text: &str, columns: usize, indent: usize, tab_size: usize) -> Vec<Range<usize>> {
    let columns = columns.max(4);
    let chars: Vec<char> = text.chars().collect();
    let mut rows = Vec::new();
    if chars.is_empty() {
        rows.push(0..0);
        return rows;
    }
    let mut start = 0;
    while start < chars.len() {
        let capacity = if start == 0 {
            columns
        } else {
            columns.saturating_sub(indent).max(4)
        };
        let mut column = if start == 0 { 0 } else { indent };
        let mut end = start;
        let mut last_break = None;
        while end < chars.len() {
            let width = if chars[end] == '\t' {
                tab_size - column % tab_size
            } else {
                1
            };
            if column + width > capacity + if start == 0 { 0 } else { indent } {
                break;
            }
            column += width;
            end += 1;
            if chars[end - 1].is_whitespace() {
                last_break = Some(end);
            }
        }
        if end == chars.len() {
            rows.push(start..end);
            break;
        }
        let cut = match last_break {
            Some(at) if at > start => at,
            _ => end.max(start + 1),
        };
        rows.push(start..cut);
        start = cut;
    }
    rows
}

/// Direct-paint element for editor tab `index` of the window.
pub struct EditorElement {
    view: Entity<ForgeWindow>,
    index: usize,
    /// Set for the active pane: registers the IME input handler over the
    /// text area, like the terminal grid does.
    input_focus: Option<gpui::FocusHandle>,
}

impl EditorElement {
    pub fn new(view: Entity<ForgeWindow>, index: usize) -> Self {
        Self {
            view,
            index,
            input_focus: None,
        }
    }

    #[must_use]
    pub fn with_input_focus(mut self, focus: gpui::FocusHandle) -> Self {
        self.input_focus = Some(focus);
        self
    }
}

impl IntoElement for EditorElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for EditorElement {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let style = Style {
            flex_grow: 1.0,
            flex_shrink: 1.0,
            flex_basis: px(0.0).into(),
            min_size: size(px(0.0).into(), px(0.0).into()),
            ..Style::default()
        };
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Self::PrepaintState {
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let index = self.index;
        if let Some(focus) = &self.input_focus {
            window.handle_input(
                focus,
                gpui::ElementInputHandler::new(bounds, self.view.clone()),
                cx,
            );
        }
        self.view.update(cx, |view, cx| {
            let metrics = view.factory.metrics;
            let theme = view.theme;
            let tab_size = view.config.editor.tab_size;
            let line_numbers = view.config.editor.line_numbers;
            let font = window.text_style().font();
            let word_wrap = view
                .tabs
                .get(index)
                .and_then(Tab::editor)
                .and_then(|editor| editor.word_wrap)
                .unwrap_or(view.config.editor.word_wrap);
            let minimap = view.minimap_override.unwrap_or(view.config.editor.minimap);
            let paint = EditorPaint {
                metrics,
                font,
                tab_size,
                line_numbers,
                foreground: color(theme.foreground).into(),
                muted: color(theme.muted).into(),
                cursor: color(theme.cursor).into(),
                selection: {
                    let mut selection: Hsla = color(theme.selection).into();
                    selection.a = theme.selection_opacity;
                    selection
                },
                syntax: theme.syntax,
                search_match: color(theme.search_match).into(),
                search_current: color(theme.search_current).into(),
                find: if view.find.open && index == view.active_tab {
                    view.find.matches.clone()
                } else {
                    Vec::new()
                },
                find_current: view.find.current,
                current_line: {
                    let mut line: Hsla = color(theme.chrome_active).into();
                    line.a = 0.6;
                    line
                },
                word_wrap,
                minimap,
                slider: {
                    let mut slider: Hsla = color(theme.muted).into();
                    slider.a = 0.25;
                    slider
                },
                ink: {
                    let mut ink: Hsla = color(theme.muted).into();
                    ink.a = 0.7;
                    ink
                },
                git_added: color(theme.git_added).into(),
                git_modified: color(theme.git_modified).into(),
                git_deleted: color(theme.git_deleted).into(),
            };
            let Some(editor) = view.tabs.get_mut(index).and_then(Tab::editor_mut) else {
                return;
            };
            paint_editor(editor, bounds, &paint, window, cx);
        });
    }
}

struct EditorPaint {
    metrics: CellMetrics,
    font: Font,
    tab_size: usize,
    line_numbers: bool,
    foreground: Hsla,
    muted: Hsla,
    cursor: Hsla,
    selection: Hsla,
    syntax: forge_gui::theme::SyntaxColors,
    search_match: Hsla,
    search_current: Hsla,
    /// Find-bar matches (char ranges) and the current one, active tab only.
    find: Vec<Range<usize>>,
    find_current: Option<usize>,
    /// Background of the cursor's line.
    current_line: Hsla,
    word_wrap: bool,
    minimap: bool,
    /// Minimap slider and ink colours.
    slider: Hsla,
    ink: Hsla,
    git_added: Hsla,
    git_modified: Hsla,
    git_deleted: Hsla,
}

impl EditorPaint {
    fn token_color(&self, token: Token) -> Hsla {
        let colors = &self.syntax;
        color(match token {
            Token::Keyword => colors.keyword,
            Token::String => colors.string,
            Token::Comment => colors.comment,
            Token::Function => colors.function,
            Token::Type => colors.type_,
            Token::Variable => colors.variable,
            Token::Number => colors.number,
            Token::Constant => colors.constant,
            Token::Operator => colors.operator,
            Token::Punctuation => colors.punctuation,
            Token::Attribute => colors.attribute,
            Token::Property => colors.property,
            Token::Tag => colors.tag,
        })
        .into()
    }

    /// Text runs for a display line from spans over the source line (byte
    /// ranges in `text`), mapped through the tab-expansion `offsets`.
    fn runs(
        &self,
        text: &str,
        offsets: &[usize],
        display_len: usize,
        spans: &[Span],
    ) -> Vec<TextRun> {
        let run = |len: usize, color: Hsla| TextRun {
            len,
            font: self.font.clone(),
            color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        if spans.is_empty() {
            return vec![run(display_len, self.foreground)];
        }
        // Byte offset in `text` → display byte offset, one entry per byte
        // so a span boundary inside a multibyte char maps to its start.
        let mut byte_to_display: Vec<usize> = Vec::with_capacity(text.len() + 1);
        for (char_index, c) in text.chars().enumerate() {
            let display = offsets.get(char_index).copied().unwrap_or(display_len);
            byte_to_display.extend(std::iter::repeat_n(display, c.len_utf8()));
        }
        byte_to_display.push(display_len);
        let to_display =
            |byte: usize| -> usize { byte_to_display.get(byte).copied().unwrap_or(display_len) };
        let mut runs = Vec::with_capacity(spans.len() * 2 + 1);
        let mut cursor = 0;
        for span in spans {
            let start = to_display(span.range.start).max(cursor);
            let end = to_display(span.range.end).max(start);
            if start > cursor {
                runs.push(run(start - cursor, self.foreground));
            }
            if end > start {
                runs.push(run(end - start, self.token_color(span.token)));
            }
            cursor = end;
        }
        if display_len > cursor {
            runs.push(run(display_len - cursor, self.foreground));
        }
        runs
    }
}

#[allow(
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]
fn paint_editor(
    editor: &mut EditorTab,
    bounds: Bounds<Pixels>,
    paint: &EditorPaint,
    window: &mut Window,
    cx: &mut App,
) {
    let line_height = px(paint.metrics.height);
    let font_size = px(paint.metrics.font_size);
    let rows = ((f32::from(bounds.size.height) / paint.metrics.height).floor() as usize).max(1);
    editor.visible_rows = rows;
    let total_lines = editor.total_lines();
    if let Some(line) = editor.pending_center.take() {
        editor.scroll_line = line.saturating_sub(rows / 2);
    }
    editor.scroll_line = editor.scroll_line.min(total_lines.saturating_sub(1));
    let digits = total_lines.max(1).to_string().len();
    let gutter = if paint.line_numbers {
        px(paint.metrics.width * digits as f32 + GUTTER_PADDING * 2.0)
    } else {
        px(GUTTER_PADDING)
    };
    let gutter = gutter + px(GIT_GUTTER_WIDTH + 2.0);
    let full_width = (bounds.size.width - gutter).max(px(0.0));
    let minimap_width = if paint.minimap
        && editor.large.is_none()
        && f32::from(full_width) > MINIMAP_MIN_TEXT_WIDTH
    {
        px(MINIMAP_WIDTH)
    } else {
        px(0.0)
    };
    let text_bounds = Bounds::new(
        bounds.origin + point(gutter, px(0.0)),
        size(full_width - minimap_width, bounds.size.height),
    );
    editor.last_text_bounds = Some(text_bounds);
    editor.minimap_bounds = (minimap_width > px(0.0)).then(|| {
        Bounds::new(
            point(text_bounds.right(), bounds.origin.y),
            size(minimap_width, bounds.size.height),
        )
    });
    let first = editor.scroll_line;
    let text_system = window.text_system().clone();
    if let Some(file) = &editor.large {
        // Read-only view: lines straight from the map, no cursor.
        let last = (first + rows).min(total_lines);
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            window.paint_layer(bounds, |window| {
                for line in first..last {
                    let y = bounds.origin.y + line_height * ((line - first) as f32);
                    let text = file.line(line, LARGE_LINE_PAINT_BYTES).unwrap_or_default();
                    let (display, _) = display_line(&text, paint.tab_size);
                    let runs = paint.runs(&text, &[], display.len(), &[]);
                    let shaped =
                        text_system.shape_line(SharedString::from(display), font_size, &runs, None);
                    let _ = shaped.paint(point(text_bounds.origin.x, y), line_height, window, cx);
                    if paint.line_numbers {
                        paint_line_number(
                            line,
                            digits,
                            paint,
                            false,
                            bounds,
                            y,
                            &text_system,
                            window,
                            cx,
                        );
                    }
                }
            });
        });
        return;
    }
    // Wrap width in columns comes from the text area; a change of width
    // or mode re-lays the rows out on the fly.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let columns =
        ((f32::from(text_bounds.size.width) / paint.metrics.width).floor() as usize).max(4);
    editor.wrap_columns = paint.word_wrap.then_some(columns);
    editor.tab_size = paint.tab_size;
    editor.scroll_x = if paint.word_wrap {
        0.0
    } else {
        editor.scroll_x.max(0.0)
    };
    let visual = editor.visual_rows(first, rows, paint.tab_size);
    let last_line = visual.last().map_or(first, |row| row.line + 1);
    let selections = editor.buffer.selections().clone();
    let primary_head = selections.primary().head;
    let primary_line = editor.buffer.position_of(primary_head).line;
    // Horizontal reveal: shape the cursor's line and slide the viewport
    // so the cursor stays a few columns inside either edge.
    if editor.reveal_x && !paint.word_wrap {
        editor.reveal_x = false;
        let text = editor.buffer.line(primary_line).unwrap_or_default();
        let (display, offsets) = display_line(&text, paint.tab_size);
        let runs = paint.runs(&text, &[], display.len(), &[]);
        let shaped = text_system.shape_line(SharedString::from(display), font_size, &runs, None);
        let column = primary_head
            - editor.buffer.char_at(Position {
                line: primary_line,
                column: 0,
            });
        let x = f32::from(shaped.x_for_index(offsets[column.min(offsets.len() - 1)]));
        let margin = paint.metrics.width * 4.0;
        let width = f32::from(text_bounds.size.width);
        if x < editor.scroll_x + margin {
            editor.scroll_x = (x - margin).max(0.0);
        } else if x > editor.scroll_x + width - margin {
            editor.scroll_x = x - width + margin;
        }
    } else if paint.word_wrap {
        editor.reveal_x = false;
    }
    let scroll_x = px(editor.scroll_x);
    // Highlights come from the tree as of the last successful parse (one
    // query for the visible range, cached until the next edit); a parse
    // that ran out of budget keeps the previous tree, so a stale frame is
    // at worst one keystroke behind.
    let rope = editor.buffer.rope();
    let highlights: Vec<Vec<Span>> = editor
        .syntax
        .as_mut()
        .map_or_else(Vec::new, |state| state.highlights(&rope, first..last_line));
    let spans_for =
        |line: usize| -> &[Span] { highlights.get(line - first).map_or(&[][..], Vec::as_slice) };
    window.with_content_mask(Some(ContentMask { bounds }), |window| {
        window.paint_layer(bounds, |window| {
            for (row_index, row) in visual.iter().enumerate() {
                let y = bounds.origin.y + line_height * (row_index as f32);
                let line = row.line;
                if line == primary_line {
                    window.paint_quad(fill(
                        Bounds::new(
                            point(bounds.origin.x, y),
                            size(bounds.size.width - minimap_width, line_height),
                        ),
                        paint.current_line,
                    ));
                }
                let text = editor.buffer.line(line).unwrap_or_default();
                let slice: String = text
                    .chars()
                    .skip(row.cols.start)
                    .take(row.cols.len())
                    .collect();
                let (display, offsets) =
                    display_line_from(&slice, paint.tab_size, row.start_column);
                // Spans are byte ranges of the whole line: shift them onto
                // the slice.
                let slice_byte_start = text
                    .char_indices()
                    .nth(row.cols.start)
                    .map_or(text.len(), |(byte, _)| byte);
                let row_spans: Vec<Span> = spans_for(line)
                    .iter()
                    .filter_map(|span| {
                        let start = span.range.start.max(slice_byte_start);
                        let end = span.range.end.min(slice_byte_start + slice.len());
                        (start < end).then(|| Span {
                            range: start - slice_byte_start..end - slice_byte_start,
                            token: span.token,
                        })
                    })
                    .collect();
                let runs = paint.runs(&slice, &offsets, display.len(), &row_spans);
                let shaped =
                    text_system.shape_line(SharedString::from(display), font_size, &runs, None);
                let x_origin = text_bounds.origin.x - scroll_x
                    + px(paint.metrics.width * row.start_column as f32);
                let line_start = editor.buffer.char_at(Position { line, column: 0 });
                let row_start = line_start + row.cols.start;
                let row_end = line_start + row.cols.end;
                let x_of = |col: usize| shaped.x_for_index(offsets[col.min(offsets.len() - 1)]);
                // Find-bar matches on this row, under the selection overlay.
                let first_match = paint.find.partition_point(|range| range.end <= row_start);
                for (index, range) in paint.find.iter().enumerate().skip(first_match) {
                    if range.start > row_end {
                        break;
                    }
                    let from = range.start.max(row_start) - row_start;
                    let to = range.end.min(row_end) - row_start;
                    let (x0, x1) = (x_of(from), x_of(to));
                    window.paint_quad(fill(
                        Bounds::new(
                            point(x_origin + x0, y),
                            size((x1 - x0).max(px(2.0)), line_height),
                        ),
                        if paint.find_current == Some(index) {
                            paint.search_current
                        } else {
                            paint.search_match
                        },
                    ));
                }
                // Selections covering this row.
                for selection in selections.iter() {
                    let range = selection.range();
                    if range.is_empty() || range.end <= row_start || range.start > row_end {
                        continue;
                    }
                    let from = range.start.max(row_start) - row_start;
                    let to = range.end.min(row_end) - row_start;
                    let x0 = x_of(from);
                    let mut x1 = x_of(to);
                    if row.last && range.end > row_end {
                        // The newline is selected too: extend to show it.
                        x1 += px(paint.metrics.width * 0.5);
                    }
                    window.paint_quad(fill(
                        Bounds::new(
                            point(x_origin + x0, y),
                            size((x1 - x0).max(px(2.0)), line_height),
                        ),
                        paint.selection,
                    ));
                }
                let _ = shaped.paint(point(x_origin, y), line_height, window, cx);
                for selection in selections.iter() {
                    let head = selection.head;
                    let on_row =
                        head >= row_start && (head < row_end || (row.last && head == row_end));
                    if !on_row {
                        continue;
                    }
                    window.paint_quad(fill(
                        Bounds::new(
                            point(x_origin + x_of(head - row_start), y),
                            size(px(2.0), line_height),
                        ),
                        paint.cursor,
                    ));
                }
                if paint.line_numbers && row.first {
                    paint_line_number(
                        line,
                        digits,
                        paint,
                        line == primary_line,
                        bounds,
                        y,
                        &text_system,
                        window,
                        cx,
                    );
                }
                if row.first
                    && let Some(mark) = editor.git.diff.mark(line)
                {
                    // VS Code's gutter: a bar for added/modified lines and a
                    // small wedge where lines were removed.
                    let x = text_bounds.origin.x - px(GIT_GUTTER_WIDTH + 2.0);
                    let (colour, height) = match mark {
                        forge_git::GutterMark::Added => (paint.git_added, line_height),
                        forge_git::GutterMark::Modified => (paint.git_modified, line_height),
                        forge_git::GutterMark::Deleted => (paint.git_deleted, px(3.0)),
                    };
                    window.paint_quad(fill(
                        Bounds::new(point(x, y), size(px(GIT_GUTTER_WIDTH), height)),
                        colour,
                    ));
                }
            }
        });
    });
    if let Some(minimap) = editor.minimap_bounds {
        paint_minimap(editor, minimap, visual.len(), paint, window);
    }
}

/// The minimap: one 2 px row per buffer line, token colours from a coarse
/// highlight query refreshed at most twice a second, and a slider over
/// the visible lines. Clicking or dragging it scrolls.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::too_many_lines
)]
fn paint_minimap(
    editor: &mut EditorTab,
    bounds: Bounds<Pixels>,
    visible_rows: usize,
    paint: &EditorPaint,
    window: &mut Window,
) {
    let total = editor.buffer.len_lines();
    let capacity = ((f32::from(bounds.size.height) / MINIMAP_ROW) as usize).max(1);
    // Keep the slider inside the strip: the strip's window slides in
    // proportion to the text's scroll position.
    let top = if total <= capacity {
        0
    } else {
        let hidden = (total - capacity) as f32;
        let scrollable = total.saturating_sub(visible_rows).max(1) as f32;
        ((editor.scroll_line as f32 / scrollable) * hidden).round() as usize
    };
    editor.minimap_top = top;
    let end = (top + capacity).min(total);
    let version = editor.buffer.version();
    let fresh = editor.minimap_cache.as_ref().is_some_and(|cache| {
        cache.lines.start <= top
            && cache.lines.end >= end
            && (cache.version == version || cache.at.elapsed() < MINIMAP_REFRESH)
    });
    if !fresh {
        let range =
            top.saturating_sub(MINIMAP_MARGIN_LINES)..(end + MINIMAP_MARGIN_LINES).min(total);
        let rope = editor.buffer.rope();
        let spans = editor
            .syntax
            .as_mut()
            .map_or_else(Vec::new, |state| state.highlights(&rope, range.clone()));
        editor.minimap_cache = Some(MinimapCache {
            at: Instant::now(),
            version,
            lines: range,
            spans,
        });
    }
    let cache = editor.minimap_cache.as_ref();
    let max_chars = (f32::from(bounds.size.width) as usize).saturating_sub(2);
    window.with_content_mask(Some(ContentMask { bounds }), |window| {
        window.paint_layer(bounds, |window| {
            for line in top..end {
                let y = bounds.origin.y + px((line - top) as f32 * MINIMAP_ROW);
                let text = editor.buffer.line(line).unwrap_or_default();
                let spans = cache
                    .filter(|cache| cache.lines.contains(&line))
                    .and_then(|cache| cache.spans.get(line - cache.lines.start));
                let mut painted = false;
                if let Some(spans) = spans {
                    for span in spans {
                        // Byte offsets to columns: ASCII-dominant code makes
                        // this exact enough for a 1 px per char strip.
                        let col0 = text[..span.range.start.min(text.len())].chars().count();
                        let col1 = text[..span.range.end.min(text.len())].chars().count();
                        if col0 >= max_chars {
                            break;
                        }
                        let width = (col1.min(max_chars) - col0).max(1);
                        window.paint_quad(fill(
                            Bounds::new(
                                point(bounds.origin.x + px(1.0 + col0 as f32), y),
                                size(px(width as f32), px(1.0)),
                            ),
                            paint.token_color(span.token),
                        ));
                        painted = true;
                    }
                }
                if !painted {
                    // No highlights: ink every non-blank run.
                    let mut run_start = None;
                    for (col, c) in text
                        .chars()
                        .chain(std::iter::once(' '))
                        .take(max_chars + 1)
                        .enumerate()
                    {
                        if c.is_whitespace() {
                            if let Some(start) = run_start.take() {
                                window.paint_quad(fill(
                                    Bounds::new(
                                        point(bounds.origin.x + px(1.0 + start as f32), y),
                                        size(px((col - start) as f32), px(1.0)),
                                    ),
                                    paint.ink,
                                ));
                            }
                        } else if run_start.is_none() {
                            run_start = Some(col);
                        }
                    }
                }
            }
            // Slider over the visible lines.
            let slider_top = editor.scroll_line.saturating_sub(top);
            let slider_rows = visible_rows.min(end.saturating_sub(editor.scroll_line));
            if editor.scroll_line >= top && slider_rows > 0 {
                window.paint_quad(fill(
                    Bounds::new(
                        point(
                            bounds.origin.x,
                            bounds.origin.y + px(slider_top as f32 * MINIMAP_ROW),
                        ),
                        size(bounds.size.width, px(slider_rows as f32 * MINIMAP_ROW)),
                    ),
                    paint.slider,
                ));
            }
        });
    });
}

#[allow(clippy::too_many_arguments)]
fn paint_line_number(
    line: usize,
    digits: usize,
    paint: &EditorPaint,
    current: bool,
    bounds: Bounds<Pixels>,
    y: Pixels,
    text_system: &gpui::WindowTextSystem,
    window: &mut Window,
    cx: &mut App,
) {
    let number = format!("{:>width$}", line + 1, width = digits);
    let run = TextRun {
        len: number.len(),
        font: paint.font.clone(),
        color: if current {
            paint.foreground
        } else {
            paint.muted
        },
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    let shaped = text_system.shape_line(
        SharedString::from(number),
        px(paint.metrics.font_size),
        std::slice::from_ref(&run),
        None,
    );
    let _ = shaped.paint(
        point(bounds.origin.x + px(GUTTER_PADDING), y),
        px(paint.metrics.height),
        window,
        cx,
    );
}

/// Types `text` at every cursor with VS Code's pairing rules: an opening
/// bracket/quote inserts its partner after the cursor, and typing the
/// closing char of an auto-inserted pair steps over it.
fn type_text(editor: &mut EditorTab, text: &str) -> Result<Transaction, BufferError> {
    let mut chars = text.chars();
    let (Some(c), None) = (chars.next(), chars.next()) else {
        return editor.buffer.insert(text, true);
    };
    let selections = editor.buffer.selections().clone();
    let all_empty = selections.iter().all(Selection::is_empty);
    // Step over a closer that is already there.
    if all_empty
        && AUTO_PAIRS.iter().any(|(_, close)| *close == c)
        && selections.iter().all(|selection| {
            editor.buffer.slice(selection.head..selection.head + 1) == c.to_string()
        })
    {
        let moved = selections.map(|selection| Selection::point(selection.head + 1));
        editor.buffer.set_selections(moved);
        return editor.buffer.insert("", true);
    }
    let Some((open, close)) = AUTO_PAIRS.iter().find(|(open, _)| *open == c).copied() else {
        return editor.buffer.insert(text, true);
    };
    // Wrap a selection; otherwise pair only before blank space or a closer,
    // and never double a quote that closes a word (`don't`).
    let wrap = !all_empty;
    let should_pair = wrap
        || selections.iter().all(|selection| {
            let next = editor.buffer.slice(selection.head..selection.head + 1);
            let prev = selection
                .head
                .checked_sub(1)
                .map(|at| editor.buffer.slice(at..selection.head))
                .unwrap_or_default();
            let next_ok = next.is_empty()
                || next
                    .chars()
                    .all(|n| n.is_whitespace() || ")]};,".contains(n));
            let prev_ok =
                open != close || prev.is_empty() || !prev.chars().all(char::is_alphanumeric);
            next_ok && prev_ok
        });
    if !should_pair {
        return editor.buffer.insert(text, true);
    }
    let edits: Vec<Edit> = selections
        .iter()
        .map(|selection| {
            let range = selection.range();
            let inner = editor.buffer.slice(range.clone());
            Edit {
                range,
                text: format!("{open}{inner}{close}"),
            }
        })
        .collect();
    // Cursors land after the opener (inside the pair), keeping wrapped text
    // selected.
    let mut drift = 0;
    let after: Vec<Selection> = selections
        .iter()
        .map(|selection| {
            let range = selection.range();
            let start = range.start + drift + 1;
            let end = start + range.len();
            drift += 2;
            if range.is_empty() {
                Selection::point(start)
            } else {
                Selection::new(start, end)
            }
        })
        .collect();
    editor.buffer.edit_with_selections(
        edits,
        Selections::new(after, selections.primary_index()),
        true,
    )
}

/// Lines covered by the selections, deduplicated and in order.
fn selected_lines(editor: &EditorTab) -> Vec<usize> {
    let mut lines: Vec<usize> = editor
        .buffer
        .selections()
        .iter()
        .flat_map(|selection| {
            let range = selection.range();
            let first = editor.buffer.position_of(range.start).line;
            // A selection ending at column 0 does not include that line.
            let end = if !range.is_empty() && editor.buffer.position_of(range.end).column == 0 {
                range.end - 1
            } else {
                range.end
            };
            first..=editor.buffer.position_of(end).line
        })
        .collect();
    lines.sort_unstable();
    lines.dedup();
    lines
}

fn line_range(editor: &EditorTab, line: usize) -> Range<usize> {
    let start = editor.buffer.char_at(Position { line, column: 0 });
    start..start + editor.buffer.line_len_chars(line)
}

/// Indents or outdents every selected line by one unit.
fn indent_lines(
    editor: &mut EditorTab,
    indent: bool,
    tab_size: usize,
    with_tabs: bool,
) -> Result<Transaction, BufferError> {
    let unit = if with_tabs {
        "\t".to_owned()
    } else {
        " ".repeat(tab_size)
    };
    let edits: Vec<Edit> = selected_lines(editor)
        .into_iter()
        .filter_map(|line| {
            let start = editor.buffer.char_at(Position { line, column: 0 });
            if indent {
                Some(Edit::insert(start, unit.clone()))
            } else {
                let text = editor.buffer.line(line)?;
                let remove = if text.starts_with('\t') {
                    1
                } else {
                    text.chars()
                        .take(tab_size)
                        .take_while(|c| *c == ' ')
                        .count()
                };
                (remove > 0).then(|| Edit::delete(start..start + remove))
            }
        })
        .collect();
    if edits.is_empty() {
        return editor.buffer.insert("", false);
    }
    editor.buffer.edit(edits, false)
}

/// Adds or removes the line-comment prefix on the selected lines, keeping
/// their indentation; all lines commented means uncomment.
fn toggle_comment(
    editor: &mut EditorTab,
    prefix: Option<&str>,
) -> Result<Transaction, BufferError> {
    let Some(prefix) = prefix else {
        return editor.buffer.insert("", false);
    };
    let lines: Vec<(usize, String)> = selected_lines(editor)
        .into_iter()
        .filter_map(|line| editor.buffer.line(line).map(|text| (line, text)))
        .filter(|(_, text)| !text.trim().is_empty())
        .collect();
    if lines.is_empty() {
        return editor.buffer.insert("", false);
    }
    let all_commented = lines
        .iter()
        .all(|(_, text)| text.trim_start().starts_with(prefix));
    let min_indent = lines
        .iter()
        .map(|(_, text)| text.chars().take_while(|c| *c == ' ' || *c == '\t').count())
        .min()
        .unwrap_or(0);
    let edits: Vec<Edit> = lines
        .into_iter()
        .map(|(line, text)| {
            let start = editor.buffer.char_at(Position { line, column: 0 });
            if all_commented {
                let indent = text.chars().take_while(|c| *c == ' ' || *c == '\t').count();
                let rest: String = text.chars().skip(indent).collect();
                let removed =
                    prefix.chars().count() + usize::from(rest[prefix.len()..].starts_with(' '));
                Edit::delete(start + indent..start + indent + removed)
            } else {
                Edit::insert(start + min_indent, format!("{prefix} "))
            }
        })
        .collect();
    editor.buffer.edit(edits, false)
}

/// Moves (or, with `duplicate`, copies) the selected lines one line down or up.
fn move_lines(
    editor: &mut EditorTab,
    down: bool,
    duplicate: bool,
) -> Result<Transaction, BufferError> {
    let lines = selected_lines(editor);
    let (Some(&first), Some(&last)) = (lines.first(), lines.last()) else {
        return editor.buffer.insert("", false);
    };
    let block_start = editor.buffer.char_at(Position {
        line: first,
        column: 0,
    });
    let block_end = line_range(editor, last).end;
    let block = editor.buffer.slice(block_start..block_end);
    let selections = editor.buffer.selections().clone();
    if duplicate {
        let text = format!("{block}\n");
        let shift = if down { text.chars().count() } else { 0 };
        let after = selections
            .map(|selection| Selection::new(selection.anchor + shift, selection.head + shift));
        return editor.buffer.edit_with_selections(
            vec![Edit::insert(block_start, text)],
            after,
            false,
        );
    }
    let total = editor.buffer.len_lines();
    if (down && last + 1 >= total) || (!down && first == 0) {
        return editor.buffer.insert("", false);
    }
    let neighbour = if down { last + 1 } else { first - 1 };
    let neighbour_range = line_range(editor, neighbour);
    let neighbour_text = editor.buffer.slice(neighbour_range.clone());
    let neighbour_len = isize::try_from(neighbour_text.chars().count() + 1).unwrap_or(0);
    let (range, text, shift) = if down {
        (
            block_start..neighbour_range.end,
            format!("{neighbour_text}\n{block}"),
            neighbour_len,
        )
    } else {
        (
            neighbour_range.start..block_end,
            format!("{block}\n{neighbour_text}"),
            -neighbour_len,
        )
    };
    let after = selections.map(|selection| {
        Selection::new(
            selection.anchor.saturating_add_signed(shift),
            selection.head.saturating_add_signed(shift),
        )
    });
    editor
        .buffer
        .edit_with_selections(vec![Edit { range, text }], after, false)
}

/// Deletes the selected lines entirely (`Ctrl+Shift+K`).
fn delete_lines(editor: &mut EditorTab) -> Result<Transaction, BufferError> {
    let lines = selected_lines(editor);
    let (Some(&first), Some(&last)) = (lines.first(), lines.last()) else {
        return editor.buffer.insert("", false);
    };
    let start = editor.buffer.char_at(Position {
        line: first,
        column: 0,
    });
    let end = if last + 1 < editor.buffer.len_lines() {
        editor.buffer.char_at(Position {
            line: last + 1,
            column: 0,
        })
    } else {
        editor.buffer.len_chars()
    };
    let start = if end == editor.buffer.len_chars() && start > 0 {
        start - 1
    } else {
        start
    };
    editor.buffer.edit_with_selections(
        vec![Edit::delete(start..end)],
        Selections::single(Selection::point(start)),
        false,
    )
}

/// Expands the primary selection to whole lines (`Ctrl+L`).
fn select_lines(editor: &mut EditorTab) {
    let selection = editor.buffer.selections().primary();
    let range = selection.range();
    let first = editor.buffer.position_of(range.start).line;
    let last_line = editor.buffer.position_of(range.end).line;
    let start = editor.buffer.char_at(Position {
        line: first,
        column: 0,
    });
    let end = if last_line + 1 < editor.buffer.len_lines() {
        editor.buffer.char_at(Position {
            line: last_line + 1,
            column: 0,
        })
    } else {
        editor.buffer.len_chars()
    };
    editor
        .buffer
        .set_selections(Selections::single(Selection::new(start, end)));
}

/// `MemAvailable` from `/proc/meminfo`; `None` elsewhere.
fn available_memory_bytes() -> Option<u64> {
    let info = std::fs::read_to_string("/proc/meminfo").ok()?;
    let line = info
        .lines()
        .find(|line| line.starts_with("MemAvailable:"))?;
    let kib: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kib * 1024)
}

impl ForgeWindow {
    // ----- opening and saving -------------------------------------------

    /// Opens `path` in an editor tab (or focuses the tab that has it) and
    /// moves the cursor to `line`/`column` (1-based) when given.
    pub fn open_file(
        &mut self,
        path: &Path,
        line: Option<u32>,
        column: Option<u32>,
        cx: &mut Context<Self>,
    ) {
        let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        if let Some(index) = self.tabs.iter().position(|tab| {
            tab.editor()
                .is_some_and(|editor| editor.path() == Some(path.as_path()))
        }) {
            self.activate_tab(index, cx);
        } else if large::is_large(&path).unwrap_or(false) {
            match LargeFile::open(&path) {
                Ok(file) => {
                    let editor = EditorTab::large(path.clone(), file);
                    self.push_tab(TabContent::Editor(Box::new(editor)), cx);
                    self.notify_user(
                        NotificationLevel::Info,
                        trf(
                            "{} opened in large-file mode (read-only, no highlighting)",
                            &[&path.display()],
                        ),
                    );
                }
                Err(error) => self.notify_user(
                    NotificationLevel::Error,
                    trf("Could not map {}: {}", &[&path.display(), &error]),
                ),
            }
        } else {
            let file = match LoadedFile::read(&path) {
                Ok(file) => file,
                Err(error) => {
                    self.notify_user(
                        NotificationLevel::Error,
                        trf("Could not open {}: {}", &[&path.display(), &error]),
                    );
                    return;
                }
            };
            if file.lossy {
                self.notify_user(
                    NotificationLevel::Warning,
                    trf("{} contains undecodable bytes", &[&path.display()]),
                );
            }
            let mut buffer = Buffer::new(&file.text);
            let mut editor = EditorTab::new(Buffer::new(""), None);
            let recovered = self.attach_journal(&mut buffer, &path);
            editor.buffer = buffer;
            editor.file = Some(file);
            editor.recovered = recovered;
            if self.config.editor.modal {
                editor.mode = EditorMode::Normal;
            }
            if recovered {
                self.notify_user(
                    NotificationLevel::Warning,
                    trf(
                        "{} recovered from the journal; save to keep the changes",
                        &[&path.display()],
                    ),
                );
            }
            let index = self.push_tab(TabContent::Editor(Box::new(editor)), cx);
            if let Some(editor) = self.tabs[index].editor_mut() {
                editor.detect_language();
            }
            self.watch_file(&path);
        }
        if let Some(line) = line
            && let Some(editor) = self.active_tab_mut().editor_mut()
        {
            editor.go_to(line, column);
        }
        cx.notify();
    }

    /// Loads a large-mode file into memory so it becomes editable.
    pub fn materialize_active(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = self.active_tab_mut().editor_mut() else {
            return;
        };
        if !editor.is_large() {
            return;
        }
        match editor.materialize() {
            Ok(()) => {
                editor.detect_language();
                if let Some(path) = editor.path().map(Path::to_path_buf) {
                    let mut buffer = std::mem::take(&mut editor.buffer);
                    let _ = self.attach_journal(&mut buffer, &path);
                    if let Some(editor) = self.active_tab_mut().editor_mut() {
                        editor.buffer = buffer;
                    }
                }
                self.notify_user(
                    NotificationLevel::Info,
                    tr("File loaded into memory; it can be edited now"),
                );
            }
            Err(error) => self.notify_user(NotificationLevel::Warning, error),
        }
        cx.notify();
    }

    /// Adopts finished parses of every editor tab; true when a repaint is
    /// due. Called from the frame-rate poll loop.
    pub fn poll_syntax(&mut self) -> bool {
        // Every tab is polled (no short circuit) so no tree stays queued.
        let mut changed = false;
        for editor in self.tabs.iter_mut().filter_map(Tab::editor_mut) {
            changed |= editor.poll_syntax();
        }
        changed
    }

    /// Replays a leftover journal for `path` into `buffer` and attaches a
    /// fresh journal. Returns whether anything was recovered.
    fn attach_journal(&mut self, buffer: &mut Buffer, path: &Path) -> bool {
        let Some(dir) = self.journal_dir() else {
            return false;
        };
        let journal_path = Journal::path_for(&dir, path);
        let mut recovered = false;
        if let Ok(transactions) = Journal::read(&journal_path) {
            for transaction in &transactions {
                if buffer.replay(transaction).is_err() {
                    break;
                }
                recovered = true;
            }
        }
        match Journal::open(&journal_path) {
            Ok(journal) => buffer.attach_journal(journal),
            Err(error) => self.notify_user(
                NotificationLevel::Warning,
                trf("No journal for {}: {}", &[&path.display(), &error]),
            ),
        }
        recovered
    }

    fn journal_dir(&self) -> Option<PathBuf> {
        self.factory
            .config_path
            .as_ref()
            .and_then(|path| path.parent())
            .map(|dir| dir.join("journal"))
    }

    pub fn new_editor_tab(&mut self, cx: &mut Context<Self>) {
        self.push_tab(
            TabContent::Editor(Box::new(EditorTab::new(Buffer::new(""), None))),
            cx,
        );
    }

    /// Saves the active editor; an untitled buffer asks for a path.
    pub fn save_active(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = self.active_tab_mut().editor_mut() else {
            return;
        };
        if editor.is_large() {
            return;
        }
        let Some(file) = &editor.file else {
            return self.save_active_as(cx);
        };
        let text = editor.buffer.text();
        let path = file.path.display().to_string();
        match file.write(&text) {
            Ok(()) => {
                editor.buffer.mark_saved();
                editor.recovered = false;
                editor.invalidate_git();
                self.notify_user(NotificationLevel::Info, format!("Guardado {path}"));
            }
            Err(error) => {
                self.notify_user(
                    NotificationLevel::Error,
                    trf("Could not save: {}", &[&error]),
                );
            }
        }
        cx.notify();
    }

    fn save_active_as(&mut self, cx: &mut Context<Self>) {
        let tab_id = self.active_tab().id;
        let receiver = cx.prompt_for_new_path(&self.factory.cwd, Some(tr("untitled.txt")));
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(path))) = receiver.await {
                let _ = this.update(cx, |view, cx| {
                    if let Some(editor) = view.tab_mut(tab_id).and_then(Tab::editor_mut) {
                        editor.file = Some(LoadedFile {
                            path: path.clone(),
                            text: String::new(),
                            line_ending: forge_buffer::LineEnding::Lf,
                            encoding: "UTF-8",
                            had_bom: false,
                            lossy: false,
                        });
                        let mut buffer = std::mem::take(&mut editor.buffer);
                        let _ = view.attach_journal(&mut buffer, &path);
                        if let Some(editor) = view.tab_mut(tab_id).and_then(Tab::editor_mut) {
                            editor.buffer = buffer;
                        }
                        let index = view.tabs.iter().position(|tab| tab.id == tab_id);
                        if let Some(index) = index {
                            if let Some(editor) = view.tabs[index].editor_mut() {
                                editor.detect_language();
                            }
                            view.activate_tab(index, cx);
                            view.save_active(cx);
                        }
                    }
                });
            }
        })
        .detach();
    }

    /// Native file picker; every chosen file opens in its own tab.
    pub fn prompt_open_file(cx: &mut Context<Self>) {
        let receiver = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some(tr("Open file").into()),
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = receiver.await {
                let _ = this.update(cx, |view, cx| {
                    for path in paths {
                        view.open_file(&path, None, None, cx);
                    }
                });
            }
        })
        .detach();
    }

    // ----- keyboard -----------------------------------------------------

    /// Editing keys; shell chords were already resolved by the caller.
    #[allow(clippy::too_many_lines)]
    pub fn editor_key(
        &mut self,
        key: &str,
        key_char: Option<&str>,
        modifiers: KeyMods,
        cx: &mut Context<Self>,
    ) {
        let (tab_size, indent_with_tabs, modal) = (
            self.config.editor.tab_size,
            self.config.editor.indent_with_tabs,
            self.config.editor.modal,
        );
        let Some(editor) = self.active_tab_mut().editor_mut() else {
            return;
        };
        if editor.is_large() {
            let rows = editor.visible_rows.saturating_sub(1).max(1);
            match key {
                "up" => editor.scroll_by(-1),
                "down" => editor.scroll_by(1),
                "pageup" => editor.scroll_by(-isize::try_from(rows).unwrap_or(1)),
                "pagedown" => editor.scroll_by(isize::try_from(rows).unwrap_or(1)),
                "home" if modifiers.control => editor.scroll_line = 0,
                "end" if modifiers.control => editor.scroll_by(isize::MAX / 2),
                _ if key_char.is_some() && !modifiers.control && !modifiers.alt => {
                    self.notify_user(
                        NotificationLevel::Info,
                        tr("Large file is read-only: use editor.materialize to edit it"),
                    );
                }
                _ => {}
            }
            cx.notify();
            return;
        }
        if editor.mode == EditorMode::Normal {
            return self.normal_mode_key(key, key_char, modifiers, cx);
        }
        let extend = modifiers.shift;
        let word = modifiers.control;
        let rows = editor.visible_rows.saturating_sub(1).max(1);
        let motion = match key {
            "left" if word => Some(Motion::WordLeft),
            "right" if word => Some(Motion::WordRight),
            "left" => Some(Motion::Left),
            "right" => Some(Motion::Right),
            "up" if !modifiers.alt => Some(Motion::Up),
            "down" if !modifiers.alt => Some(Motion::Down),
            "home" if modifiers.control => Some(Motion::DocumentStart),
            "end" if modifiers.control => Some(Motion::DocumentEnd),
            "home" => Some(Motion::LineStart),
            "end" => Some(Motion::LineEnd),
            "pageup" => Some(Motion::PageUp(rows)),
            "pagedown" => Some(Motion::PageDown(rows)),
            _ => None,
        };
        if let Some(motion) = motion {
            let goal = editor.goal_column;
            let vertical = matches!(
                motion,
                Motion::Up | Motion::Down | Motion::PageUp(_) | Motion::PageDown(_)
            );
            let mut new_goal = None;
            let selections = editor.buffer.selections().clone().map(|selection| {
                let cursor = editor.buffer.apply_motion(
                    Cursor {
                        selection,
                        goal_column: if vertical { goal } else { None },
                    },
                    motion,
                    extend,
                );
                new_goal = cursor.goal_column;
                cursor.selection
            });
            editor.buffer.set_selections(selections);
            editor.goal_column = if vertical { new_goal } else { None };
            editor.follow_cursor();
            cx.notify();
            return;
        }
        let language = editor.syntax.as_ref().map(SyntaxState::language_name);
        let result = match key {
            "backspace" if modifiers.control => {
                let edits = editor
                    .buffer
                    .selections()
                    .iter()
                    .map(|selection| {
                        let range = selection.range();
                        let start = if range.is_empty() {
                            editor.buffer.word_boundary_left(range.start)
                        } else {
                            range.start
                        };
                        Edit::delete(start..range.end)
                    })
                    .collect();
                editor.buffer.edit(edits, true).map(Some)
            }
            "delete" if modifiers.control => {
                let edits = editor
                    .buffer
                    .selections()
                    .iter()
                    .map(|selection| {
                        let range = selection.range();
                        let end = if range.is_empty() {
                            editor.buffer.word_boundary_right(range.end)
                        } else {
                            range.end
                        };
                        Edit::delete(range.start..end)
                    })
                    .collect();
                editor.buffer.edit(edits, true).map(Some)
            }
            "backspace" => {
                // Backspace between an auto-closed pair removes both.
                let pair = editor.buffer.selections().primary();
                let inside_pair = pair.is_empty()
                    && pair.head > 0
                    && AUTO_PAIRS.iter().any(|(open, close)| {
                        editor.buffer.slice(pair.head - 1..pair.head) == open.to_string()
                            && editor.buffer.slice(pair.head..pair.head + 1) == close.to_string()
                    });
                if inside_pair && editor.buffer.selections().len() == 1 {
                    editor.buffer.delete(1, 1).map(Some)
                } else {
                    editor.buffer.delete(1, 0).map(Some)
                }
            }
            "delete" => editor.buffer.delete(0, 1).map(Some),
            "enter" => {
                // Inherit the indentation; between `{` and `}` open a block.
                let head = editor.buffer.selections().primary().head;
                let line = editor.buffer.position_of(head).line;
                let indent: String = editor
                    .buffer
                    .line(line)
                    .unwrap_or_default()
                    .chars()
                    .take_while(|c| *c == ' ' || *c == '\t')
                    .collect();
                let before = head.checked_sub(1).map(|at| editor.buffer.slice(at..head));
                let after = editor.buffer.slice(head..head + 1);
                let unit = if indent_with_tabs {
                    "\t".to_owned()
                } else {
                    " ".repeat(tab_size)
                };
                let opens_block = matches!(before.as_deref(), Some("{" | "(" | "["));
                if opens_block && editor.buffer.selections().len() == 1 {
                    let closes = matches!(after.as_str(), "}" | ")" | "]");
                    let text = if closes {
                        format!("\n{indent}{unit}\n{indent}")
                    } else {
                        format!("\n{indent}{unit}")
                    };
                    let cursor = head + 1 + indent.chars().count() + unit.chars().count();
                    editor
                        .buffer
                        .edit_with_selections(
                            vec![Edit::insert(head, text)],
                            Selections::single(Selection::point(cursor)),
                            false,
                        )
                        .map(Some)
                } else {
                    editor
                        .buffer
                        .insert(&format!("\n{indent}"), false)
                        .map(Some)
                }
            }
            "tab"
                if modifiers.shift
                    || editor.buffer.selections().iter().any(|selection| {
                        !selection.is_empty()
                            && editor.buffer.slice(selection.range()).contains('\n')
                    }) =>
            {
                indent_lines(editor, !modifiers.shift, tab_size, indent_with_tabs).map(Some)
            }
            "tab" => {
                let text = if indent_with_tabs {
                    "\t".to_owned()
                } else {
                    let column = editor
                        .buffer
                        .position_of(editor.buffer.selections().primary().head)
                        .column;
                    " ".repeat(tab_size - column % tab_size)
                };
                editor.buffer.insert(&text, true).map(Some)
            }
            "escape" => {
                let collapsed = editor.buffer.selections().clone().collapse_to_primary();
                editor.buffer.set_selections(collapsed);
                if modal {
                    editor.mode = EditorMode::Normal;
                    editor.extending = false;
                }
                Ok(None)
            }
            "/" if modifiers.control => toggle_comment(editor, line_comment(language)).map(Some),
            "up" | "down" if modifiers.alt && !modifiers.control => {
                move_lines(editor, key == "down", modifiers.shift).map(Some)
            }
            "k" if modifiers.control && modifiers.shift => delete_lines(editor).map(Some),
            "l" if modifiers.control => {
                select_lines(editor);
                Ok(None)
            }
            "]" | "[" if modifiers.control => {
                indent_lines(editor, key == "]", tab_size, indent_with_tabs).map(Some)
            }
            _ if !modifiers.control && !modifiers.alt => match key_char {
                Some(text) if !text.is_empty() => type_text(editor, text).map(Some),
                _ => return,
            },
            _ => return,
        };
        if let Err(error) = result {
            self.notify_user(NotificationLevel::Error, trf("Edit failed: {}", &[&error]));
        } else if let Some(editor) = self.active_tab_mut().editor_mut() {
            editor.goal_column = None;
            editor.follow_cursor();
        }
        cx.notify();
    }

    /// Normal-mode keys of the modal keymap (a Helix subset).
    #[allow(clippy::too_many_lines)]
    fn normal_mode_key(
        &mut self,
        key: &str,
        key_char: Option<&str>,
        modifiers: KeyMods,
        cx: &mut Context<Self>,
    ) {
        let Some(editor) = self.active_tab_mut().editor_mut() else {
            return;
        };
        let rows = editor.visible_rows.saturating_sub(1).max(1);
        let extend = editor.extending || modifiers.shift;
        let motion = match key_char.filter(|_| !modifiers.control && !modifiers.alt) {
            Some("h") => Some(Motion::Left),
            Some("l") => Some(Motion::Right),
            Some("k") => Some(Motion::Up),
            Some("j") => Some(Motion::Down),
            Some("w") => Some(Motion::WordRight),
            Some("b") => Some(Motion::WordLeft),
            Some("0") => Some(Motion::LineStart),
            Some("$") => Some(Motion::LineEnd),
            Some("G") => Some(Motion::DocumentEnd),
            _ => match key {
                "left" => Some(Motion::Left),
                "right" => Some(Motion::Right),
                "up" => Some(Motion::Up),
                "down" => Some(Motion::Down),
                "home" => Some(Motion::LineStart),
                "end" => Some(Motion::LineEnd),
                "pageup" => Some(Motion::PageUp(rows)),
                "pagedown" => Some(Motion::PageDown(rows)),
                _ => None,
            },
        };
        if let Some(motion) = motion {
            let goal = editor.goal_column;
            let vertical = matches!(
                motion,
                Motion::Up | Motion::Down | Motion::PageUp(_) | Motion::PageDown(_)
            );
            let mut new_goal = None;
            let selections = editor.buffer.selections().clone().map(|selection| {
                let cursor = editor.buffer.apply_motion(
                    Cursor {
                        selection,
                        goal_column: if vertical { goal } else { None },
                    },
                    motion,
                    extend,
                );
                new_goal = cursor.goal_column;
                cursor.selection
            });
            editor.buffer.set_selections(selections);
            editor.goal_column = if vertical { new_goal } else { None };
            editor.follow_cursor();
            cx.notify();
            return;
        }
        let mut changed = false;
        match (key, key_char) {
            ("escape", _) => {
                let collapsed = editor.buffer.selections().clone().collapse_to_primary();
                editor.buffer.set_selections(collapsed);
                editor.extending = false;
            }
            (_, Some("i")) => editor.mode = EditorMode::Insert,
            (_, Some("a")) => {
                let moved = editor.buffer.selections().clone().map(|selection| {
                    Selection::point((selection.range().end).min(editor.buffer.len_chars()))
                });
                editor.buffer.set_selections(moved);
                editor.mode = EditorMode::Insert;
            }
            (_, Some("o" | "O")) => {
                let below = key_char == Some("o");
                let line = editor
                    .buffer
                    .position_of(editor.buffer.selections().primary().head)
                    .line;
                let indent: String = editor
                    .buffer
                    .line(line)
                    .unwrap_or_default()
                    .chars()
                    .take_while(|c| *c == ' ' || *c == '\t')
                    .collect();
                let (at, text) = if below {
                    (
                        editor.buffer.char_at(Position {
                            line,
                            column: usize::MAX,
                        }),
                        format!("\n{indent}"),
                    )
                } else {
                    (
                        editor.buffer.char_at(Position { line, column: 0 }),
                        format!("{indent}\n"),
                    )
                };
                let after = if below {
                    at + 1 + indent.chars().count()
                } else {
                    at + indent.chars().count()
                };
                changed = editor
                    .buffer
                    .edit_with_selections(
                        vec![Edit::insert(at, text)],
                        Selections::single(Selection::point(after)),
                        false,
                    )
                    .is_ok();
                editor.mode = EditorMode::Insert;
            }
            (_, Some("v")) => editor.extending = !editor.extending,
            (_, Some("x")) => changed = editor.buffer.delete(0, 1).is_ok(),
            (_, Some("d")) => {
                let edits: Vec<Edit> = editor
                    .buffer
                    .selections()
                    .iter()
                    .filter(|selection| !selection.is_empty())
                    .map(|selection| Edit::delete(selection.range()))
                    .collect();
                changed = !edits.is_empty() && editor.buffer.edit(edits, false).is_ok();
                editor.extending = false;
            }
            (_, Some("u")) => changed = editor.buffer.undo(),
            (_, Some("U")) => changed = editor.buffer.redo(),
            (_, Some("y")) => return self.editor_copy(false, cx),
            (_, Some("p")) => return self.editor_paste(cx),
            (_, Some("/")) => return self.open_find(false, cx),
            _ => {}
        }
        if changed {
            editor.goal_column = None;
            editor.follow_cursor();
            editor.sync_syntax();
        }
        self.refresh_find();
        cx.notify();
    }

    /// Saves every dirty file whose last edit is older than the configured
    /// autosave delay. Returns whether anything was written.
    pub fn autosave(&mut self) -> bool {
        let delay = self.config.editor.autosave_ms;
        if delay == 0 {
            return false;
        }
        let delay = Duration::from_millis(delay);
        let mut saved = Vec::new();
        for tab in &mut self.tabs {
            let Some(editor) = tab.editor_mut() else {
                continue;
            };
            let Some(file) = &editor.file else { continue };
            if editor.is_large() || !editor.buffer.is_dirty() || editor.last_edit.elapsed() < delay
            {
                continue;
            }
            if file.write(&editor.buffer.text()).is_ok() {
                editor.buffer.mark_saved();
                editor.recovered = false;
                saved.push(file.path.display().to_string());
            }
        }
        for path in &saved {
            self.notify_user(NotificationLevel::Info, format!("Autoguardado {path}"));
        }
        !saved.is_empty()
    }

    /// Inserts text at the cursors of the active editor without a GPUI
    /// context; the benchmark harness redraws on its own.
    pub fn editor_type(&mut self, text: &str) {
        if let Some(editor) = self.active_tab_mut().editor_mut() {
            let _ = editor.buffer.insert(text, true);
            editor.follow_cursor();
            editor.sync_syntax();
        }
    }

    /// Text committed by the IME (dead keys, CJK) into the active editor.
    pub fn editor_insert_text(&mut self, text: &str, cx: &mut Context<Self>) {
        if let Some(editor) = self.active_tab_mut().editor_mut() {
            let _ = editor.buffer.insert(text, true);
            editor.goal_column = None;
            editor.follow_cursor();
            editor.sync_syntax();
            cx.notify();
        }
    }

    pub fn editor_undo(&mut self, redo: bool, cx: &mut Context<Self>) {
        if let Some(editor) = self.active_tab_mut().editor_mut() {
            let changed = if redo {
                editor.buffer.redo()
            } else {
                editor.buffer.undo()
            };
            if changed {
                editor.follow_cursor();
                editor.sync_syntax();
                self.refresh_find();
                cx.notify();
            }
        }
    }

    pub fn editor_select_all(&mut self, cx: &mut Context<Self>) {
        if let Some(editor) = self.active_tab_mut().editor_mut() {
            let len = editor.buffer.len_chars();
            editor
                .buffer
                .set_selections(Selections::single(Selection::new(0, len)));
            cx.notify();
        }
    }

    /// Adds a cursor on the line above/below the last one (`Ctrl+Alt+↑/↓`).
    pub fn editor_add_cursor(&mut self, below: bool, cx: &mut Context<Self>) {
        if let Some(editor) = self.active_tab_mut().editor_mut() {
            let mut selections = editor.buffer.selections().clone();
            let edge = if below {
                selections.iter().last().copied()
            } else {
                selections.iter().next().copied()
            };
            if let Some(edge) = edge {
                let moved = editor.buffer.apply_motion(
                    Cursor {
                        selection: Selection::point(edge.head),
                        goal_column: editor.goal_column,
                    },
                    if below { Motion::Down } else { Motion::Up },
                    false,
                );
                selections.push(moved.selection);
                editor.buffer.set_selections(selections);
                editor.follow_cursor();
                cx.notify();
            }
        }
    }

    /// Selects the word under the primary cursor, or adds the next
    /// occurrence of the current selection as another cursor (`Ctrl+D`).
    pub fn editor_select_next_match(&mut self, cx: &mut Context<Self>) {
        if let Some(editor) = self.active_tab_mut().editor_mut() {
            let mut selections = editor.buffer.selections().clone();
            let primary = selections.primary();
            if primary.is_empty() {
                let word = editor.buffer.word_at(primary.head);
                if !word.is_empty() {
                    editor.buffer.set_selections(Selections::single(word));
                    cx.notify();
                }
                return;
            }
            let needle = editor.buffer.slice(primary.range());
            let text = editor.buffer.text();
            let last_end = selections
                .iter()
                .map(|selection| selection.range().end)
                .max()
                .unwrap_or(0);
            let from_byte = editor.buffer.byte_of(last_end);
            let found = text[from_byte..]
                .find(&needle)
                .map(|offset| from_byte + offset)
                .or_else(|| text.find(&needle));
            if let Some(byte) = found {
                let start = editor.buffer.char_of(byte);
                let end = start + needle.chars().count();
                selections.push(Selection::new(start, end));
                editor.buffer.set_selections(selections);
                editor.follow_cursor();
                cx.notify();
            }
        }
    }

    pub fn editor_copy(&mut self, cut: bool, cx: &mut Context<Self>) {
        let Some(editor) = self.active_tab_mut().editor_mut() else {
            return;
        };
        let pieces: Vec<String> = editor
            .buffer
            .selections()
            .iter()
            .map(|selection| {
                if selection.is_empty() {
                    // Empty selection copies the whole line, like VS Code.
                    let line = editor.buffer.position_of(selection.head).line;
                    editor.buffer.line(line).unwrap_or_default() + "\n"
                } else {
                    editor.buffer.slice(selection.range())
                }
            })
            .collect();
        cx.write_to_clipboard(ClipboardItem::new_string(pieces.join("\n")));
        if cut {
            let edits: Vec<Edit> = editor
                .buffer
                .selections()
                .iter()
                .map(|selection| {
                    if selection.is_empty() {
                        let line = editor.buffer.position_of(selection.head).line;
                        let start = editor.buffer.char_at(Position { line, column: 0 });
                        let end = if line + 1 < editor.buffer.len_lines() {
                            editor.buffer.char_at(Position {
                                line: line + 1,
                                column: 0,
                            })
                        } else {
                            editor.buffer.len_chars()
                        };
                        Edit::delete(start..end)
                    } else {
                        Edit::delete(selection.range())
                    }
                })
                .collect();
            let _ = editor.buffer.edit(edits, false);
            editor.follow_cursor();
            editor.sync_syntax();
            cx.notify();
        }
    }

    pub fn editor_paste(&mut self, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return;
        };
        if let Some(editor) = self.active_tab_mut().editor_mut() {
            let _ = editor.buffer.insert(&text, false);
            editor.follow_cursor();
            editor.sync_syntax();
            cx.notify();
        }
    }

    // ----- mouse --------------------------------------------------------

    /// Char index under a window position in the active editor.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    fn editor_char_at(&self, position: Point<Pixels>, window: &Window) -> Option<usize> {
        let editor = self
            .active_tab()
            .editor()
            .filter(|editor| !editor.is_large())?;
        let bounds = editor.last_text_bounds?;
        let metrics = self.factory.metrics;
        let tab_size = self.config.editor.tab_size;
        let row_index = ((f32::from(position.y - bounds.origin.y)) / metrics.height)
            .floor()
            .max(0.0) as usize;
        let rows = editor.visual_rows(editor.scroll_line, row_index + 1, tab_size);
        let row = rows.last()?.clone();
        let text = editor.buffer.line(row.line).unwrap_or_default();
        let slice: String = text
            .chars()
            .skip(row.cols.start)
            .take(row.cols.len())
            .collect();
        let (display, offsets) = display_line_from(&slice, tab_size, row.start_column);
        let run = TextRun {
            len: display.len(),
            font: window.text_style().font(),
            color: Hsla::default(),
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let shaped = window.text_system().shape_line(
            SharedString::from(display),
            px(metrics.font_size),
            std::slice::from_ref(&run),
            None,
        );
        let x_origin =
            bounds.origin.x - px(editor.scroll_x) + px(metrics.width * row.start_column as f32);
        let x = (position.x - x_origin).max(px(0.0));
        let byte = shaped.closest_index_for_x(x);
        let column = offsets
            .partition_point(|offset| *offset < byte)
            .min(slice.chars().count());
        let column = if column > 0 && offsets[column] > byte {
            column - 1
        } else {
            column
        };
        let line_start = editor.buffer.char_at(Position {
            line: row.line,
            column: 0,
        });
        Some(line_start + row.cols.start + column)
    }

    pub fn editor_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .active_tab()
            .editor()
            .is_some_and(|editor| editor.on_minimap(event.position))
        {
            if let Some(editor) = self.active_tab_mut().editor_mut() {
                // A click jumps there; holding the button then drags the
                // slider at document speed.
                editor.scroll_to_minimap(event.position.y);
                editor.begin_minimap_drag(event.position.y);
            }
            cx.notify();
            return;
        }
        let Some(at) = self.editor_char_at(event.position, window) else {
            return;
        };
        let Some(editor) = self.active_tab_mut().editor_mut() else {
            return;
        };
        let mut selections = editor.buffer.selections().clone();
        match event.click_count {
            2 => {
                let word = editor.buffer.word_at(at);
                selections = Selections::single(word);
            }
            n if n >= 3 => {
                let line = editor.buffer.position_of(at).line;
                let start = editor.buffer.char_at(Position { line, column: 0 });
                let end = start + editor.buffer.line_len_chars(line);
                selections = Selections::single(Selection::new(start, end));
            }
            _ if event.modifiers.shift => {
                let primary = selections.primary();
                selections = Selections::single(Selection::new(primary.anchor, at));
            }
            _ if event.modifiers.alt => selections.push(Selection::point(at)),
            _ => selections = Selections::single(Selection::point(at)),
        }
        editor.buffer.set_selections(selections);
        editor.goal_column = None;
        editor.drag_anchor = Some(at);
        cx.notify();
    }

    pub fn editor_mouse_drag(
        &mut self,
        position: Point<Pixels>,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .active_tab()
            .editor()
            .is_some_and(|editor| editor.dragging_minimap)
        {
            if let Some(editor) = self.active_tab_mut().editor_mut() {
                editor.drag_minimap(position.y);
            }
            cx.notify();
            return;
        }
        let Some(anchor) = self
            .active_tab()
            .editor()
            .and_then(|editor| editor.drag_anchor)
        else {
            return;
        };
        let Some(at) = self.editor_char_at(position, window) else {
            return;
        };
        if let Some(editor) = self.active_tab_mut().editor_mut() {
            let mut selections = editor.buffer.selections().clone();
            let index = selections.primary_index();
            let mut list = selections.as_slice().to_vec();
            list[index] = Selection::new(anchor, at);
            selections = Selections::new(list, index);
            if *editor.buffer.selections() != selections {
                editor.buffer.set_selections(selections);
                cx.notify();
            }
        }
    }

    pub fn editor_mouse_up(&mut self) {
        if let Some(editor) = self.active_tab_mut().editor_mut() {
            editor.drag_anchor = None;
            editor.dragging_minimap = false;
            editor.minimap_drag = None;
        }
    }

    /// Polls every editor for a due gutter diff; called each tick.
    pub fn refresh_git_diffs(&mut self) {
        let events = self.event_tx.clone();
        for tab in &mut self.tabs {
            let id = tab.id;
            if let Some(editor) = tab.editor_mut() {
                editor.refresh_git(id, &events);
            }
        }
    }

    pub fn on_git_diff(
        &mut self,
        tab_id: u64,
        version: u64,
        result: GitDiffResult,
        cx: &mut Context<Self>,
    ) {
        if let Some(editor) = self.tab_mut(tab_id).and_then(Tab::editor_mut)
            && editor.git_ready(version, result)
        {
            cx.notify();
        }
    }

    /// Horizontal wheel (or Shift+wheel) in no-wrap mode.
    pub fn editor_scroll_x(&mut self, delta: f32, cx: &mut Context<Self>) {
        if let Some(editor) = self.active_tab_mut().editor_mut()
            && editor.wrap_columns.is_none()
        {
            editor.scroll_x = (editor.scroll_x + delta).max(0.0);
            cx.notify();
        }
    }

    pub fn toggle_word_wrap(&mut self, cx: &mut Context<Self>) {
        let default = self.config.editor.word_wrap;
        if let Some(editor) = self.active_tab_mut().editor_mut() {
            let current = editor.word_wrap.unwrap_or(default);
            editor.word_wrap = Some(!current);
            editor.scroll_x = 0.0;
            editor.follow_cursor();
            cx.notify();
        }
    }

    pub fn toggle_minimap(&mut self, cx: &mut Context<Self>) {
        let current = self.minimap_override.unwrap_or(self.config.editor.minimap);
        self.minimap_override = Some(!current);
        cx.notify();
    }

    /// Cursor rectangle for IME candidate windows.
    #[allow(clippy::cast_precision_loss)]
    pub fn editor_cursor_bounds(&self, window: &Window) -> Option<Bounds<Pixels>> {
        let editor = self
            .active_tab()
            .editor()
            .filter(|editor| !editor.is_large())?;
        let bounds = editor.last_text_bounds?;
        let head = editor.buffer.selections().primary().head;
        let position = editor.buffer.position_of(head);
        if position.line < editor.scroll_line {
            return None;
        }
        let metrics = self.factory.metrics;
        let text = editor.buffer.line(position.line).unwrap_or_default();
        let (display, offsets) = display_line(&text, self.config.editor.tab_size);
        let run = TextRun {
            len: display.len(),
            font: window.text_style().font(),
            color: Hsla::default(),
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let shaped = window.text_system().shape_line(
            SharedString::from(display),
            px(metrics.font_size),
            std::slice::from_ref(&run),
            None,
        );
        let x = shaped.x_for_index(offsets[position.column.min(offsets.len() - 1)])
            - px(editor.scroll_x);
        let y = px(metrics.height * (position.line - editor.scroll_line) as f32);
        Some(Bounds::new(
            bounds.origin + point(x, y),
            size(px(2.0), px(metrics.height)),
        ))
    }
}

/// Line range visible in an editor, for the scrollbar.
pub fn visible_range(editor: &EditorTab) -> Range<usize> {
    editor.scroll_line..(editor.scroll_line + editor.visible_rows).min(editor.buffer.len_lines())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn editor(text: &str, at: usize) -> EditorTab {
        let mut editor = EditorTab::new(Buffer::new(text), None);
        editor
            .buffer
            .set_selections(Selections::single(Selection::point(at)));
        editor
    }

    #[test]
    fn brackets_and_quotes_auto_close_and_step_over() {
        let mut tab = editor("call", 4);
        type_text(&mut tab, "(").unwrap();
        assert_eq!(tab.buffer.text(), "call()");
        assert_eq!(tab.buffer.selections().primary().head, 5);
        type_text(&mut tab, "x").unwrap();
        type_text(&mut tab, ")").unwrap();
        assert_eq!(
            tab.buffer.text(),
            "call(x)",
            "the closer is stepped over, not doubled"
        );
        assert_eq!(tab.buffer.selections().primary().head, 7);
        // A quote after a word closes the word instead of pairing.
        let mut tab = editor("don", 3);
        type_text(&mut tab, "'").unwrap();
        assert_eq!(tab.buffer.text(), "don'");
        // Pairs are not inserted before text.
        let mut tab = editor("x", 0);
        type_text(&mut tab, "(").unwrap();
        assert_eq!(tab.buffer.text(), "(x");
        // A selection gets wrapped and stays selected.
        let mut tab = editor("word", 0);
        tab.buffer
            .set_selections(Selections::single(Selection::new(0, 4)));
        type_text(&mut tab, "\"").unwrap();
        assert_eq!(tab.buffer.text(), "\"word\"");
        assert_eq!(tab.buffer.selections().primary(), Selection::new(1, 5));
    }

    #[test]
    fn line_operations_follow_vscode() {
        let mut tab = editor("a\n    b\nc\n", 6);
        toggle_comment(&mut tab, Some("//")).unwrap();
        assert_eq!(tab.buffer.text(), "a\n    // b\nc\n");
        toggle_comment(&mut tab, Some("//")).unwrap();
        assert_eq!(tab.buffer.text(), "a\n    b\nc\n");
        indent_lines(&mut tab, true, 4, false).unwrap();
        assert_eq!(tab.buffer.text(), "a\n        b\nc\n");
        indent_lines(&mut tab, false, 4, false).unwrap();
        indent_lines(&mut tab, false, 4, false).unwrap();
        assert_eq!(tab.buffer.text(), "a\nb\nc\n");
        move_lines(&mut tab, false, false).unwrap();
        assert_eq!(tab.buffer.text(), "b\na\nc\n");
        assert_eq!(
            tab.buffer
                .position_of(tab.buffer.selections().primary().head)
                .line,
            0
        );
        move_lines(&mut tab, true, false).unwrap();
        assert_eq!(tab.buffer.text(), "a\nb\nc\n");
        move_lines(&mut tab, true, true).unwrap();
        assert_eq!(tab.buffer.text(), "a\nb\nb\nc\n");
        assert_eq!(
            tab.buffer
                .position_of(tab.buffer.selections().primary().head)
                .line,
            2
        );
        delete_lines(&mut tab).unwrap();
        assert_eq!(tab.buffer.text(), "a\nb\nc\n");
        select_lines(&mut tab);
        assert_eq!(
            tab.buffer.slice(tab.buffer.selections().primary().range()),
            "c\n",
            "the cursor stays on the same line number after a delete"
        );
        assert!(
            toggle_comment(&mut tab, None).is_ok(),
            "no comment syntax is a no-op"
        );
    }

    #[test]
    fn lines_wrap_at_word_boundaries_and_keep_the_indent() {
        assert_eq!(wrap_line("", 10, 0, 4), vec![0..0]);
        assert_eq!(wrap_line("short", 10, 0, 4), vec![0..5]);
        let rows = wrap_line("one two three four", 9, 0, 4);
        assert_eq!(rows, vec![0..8, 8..14, 14..18], "breaks after spaces");
        let rows = wrap_line("abcdefghijkl", 5, 0, 4);
        assert_eq!(
            rows,
            vec![0..5, 5..10, 10..12],
            "hard breaks without spaces"
        );
        // Continuations keep two columns of indent, so they hold fewer chars.
        let rows = wrap_line("  aaaa bbbb cccc", 8, 2, 4);
        assert_eq!(rows, vec![0..7, 7..12, 12..16]);
    }

    #[test]
    fn tabs_expand_to_the_next_stop_and_offsets_map_chars_to_display_bytes() {
        let (display, offsets) = display_line("a\tbé\tc", 4);
        assert_eq!(display, "a   bé  c");
        assert_eq!(offsets, [0, 1, 4, 5, 7, 9, 10]);
        let (display, offsets) = display_line("", 4);
        assert_eq!(display, "");
        assert_eq!(offsets, [0]);
    }
}
