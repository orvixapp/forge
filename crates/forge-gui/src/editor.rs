//! Editor tabs: a [`forge_buffer::Buffer`] painted line by line with GPUI's
//! text shaper, plus the keyboard and mouse handling that turns keystrokes
//! into transactions. Terminal tabs keep their own element in
//! `grid_element.rs`; both share the pane tree.

use crate::{
    grid_element::{CellMetrics, color},
    window::{ForgeWindow, NotificationLevel, Tab, TabContent},
};
use forge_buffer::{
    Buffer, Cursor, Edit, Journal, LargeFile, LoadedFile, Motion, Position, Selection, Selections,
    large,
};
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
            return Err(format!(
                "el archivo ocupa {} MiB y sólo hay {} MiB disponibles; no se carga en memoria",
                needed / (1024 * 1024),
                available / (1024 * 1024)
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
            None => Cow::Borrowed("Sin título"),
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
        if self.recovered {
            parts.push("recuperado del journal".into());
        }
        if self.buffer.is_dirty() {
            parts.push("● sin guardar".into());
        }
        parts.join(" · ")
    }

    /// Whether a window position falls on the text area.
    pub fn contains(&self, position: Point<Pixels>) -> bool {
        self.last_text_bounds
            .is_some_and(|bounds| bounds.contains(&position))
    }

    /// Keeps the primary cursor inside the visible rows.
    pub fn follow_cursor(&mut self) {
        let line = self
            .buffer
            .position_of(self.buffer.selections().primary().head)
            .line;
        let rows = self.visible_rows.max(1);
        if line < self.scroll_line {
            self.scroll_line = line;
        } else if line >= self.scroll_line + rows {
            self.scroll_line = line + 1 - rows;
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
    let mut display = String::with_capacity(text.len());
    let mut offsets = Vec::with_capacity(text.len() + 1);
    let mut column = 0;
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

/// Direct-paint element for editor tab `index` of the window.
pub struct EditorElement {
    view: Entity<ForgeWindow>,
    index: usize,
}

impl EditorElement {
    pub fn new(view: Entity<ForgeWindow>, index: usize) -> Self {
        Self { view, index }
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
        self.view.update(cx, |view, cx| {
            let metrics = view.factory.metrics;
            let theme = view.theme;
            let tab_size = view.config.editor.tab_size;
            let line_numbers = view.config.editor.line_numbers;
            let font = window.text_style().font();
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
    let text_bounds = Bounds::new(
        bounds.origin + point(gutter, px(0.0)),
        size(
            (bounds.size.width - gutter).max(px(0.0)),
            bounds.size.height,
        ),
    );
    editor.last_text_bounds = Some(text_bounds);
    let first = editor.scroll_line;
    let last = (first + rows).min(total_lines);
    let text_system = window.text_system().clone();
    if let Some(file) = &editor.large {
        // Read-only view: lines straight from the map, no cursor.
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
    let selections = editor.buffer.selections().clone();
    let primary_line = editor.buffer.position_of(selections.primary().head).line;
    // Highlights come from the tree as of the last successful parse (one
    // query for the visible range, cached until the next edit); a parse
    // that ran out of budget keeps the previous tree, so a stale frame is
    // at worst one keystroke behind.
    let rope = editor.buffer.rope();
    let highlights: Vec<Vec<Span>> = editor
        .syntax
        .as_mut()
        .map_or_else(Vec::new, |state| state.highlights(&rope, first..last));
    let spans_for =
        |line: usize| -> &[Span] { highlights.get(line - first).map_or(&[][..], Vec::as_slice) };
    window.with_content_mask(Some(ContentMask { bounds }), |window| {
        window.paint_layer(bounds, |window| {
            for line in first..last {
                let y = bounds.origin.y + line_height * ((line - first) as f32);
                let text = editor.buffer.line(line).unwrap_or_default();
                let (display, offsets) = display_line(&text, paint.tab_size);
                let runs = paint.runs(&text, &offsets, display.len(), spans_for(line));
                let shaped =
                    text_system.shape_line(SharedString::from(display), font_size, &runs, None);
                let line_start = editor.buffer.char_at(Position { line, column: 0 });
                let line_end = line_start + editor.buffer.line_len_chars(line);
                // Find-bar matches on this line, under the selection overlay.
                let first_match = paint.find.partition_point(|range| range.end <= line_start);
                for (index, range) in paint.find.iter().enumerate().skip(first_match) {
                    if range.start > line_end {
                        break;
                    }
                    let from = range.start.max(line_start) - line_start;
                    let to = range.end.min(line_end) - line_start;
                    let x0 = shaped.x_for_index(offsets[from]);
                    let x1 = shaped.x_for_index(offsets[to]);
                    window.paint_quad(fill(
                        Bounds::new(
                            point(text_bounds.origin.x + x0, y),
                            size((x1 - x0).max(px(2.0)), line_height),
                        ),
                        if paint.find_current == Some(index) {
                            paint.search_current
                        } else {
                            paint.search_match
                        },
                    ));
                }
                // Selections covering this line, as x ranges of the display text.
                for selection in selections.iter() {
                    let range = selection.range();
                    if range.is_empty() || range.end <= line_start || range.start > line_end {
                        continue;
                    }
                    let from = range.start.max(line_start) - line_start;
                    let to = range.end.min(line_end) - line_start;
                    let x0 = shaped.x_for_index(offsets[from]);
                    let mut x1 = shaped.x_for_index(offsets[to]);
                    if range.end > line_end {
                        // The newline is selected too: extend to show it.
                        x1 += px(paint.metrics.width * 0.5);
                    }
                    window.paint_quad(fill(
                        Bounds::new(
                            point(text_bounds.origin.x + x0, y),
                            size((x1 - x0).max(px(2.0)), line_height),
                        ),
                        paint.selection,
                    ));
                }
                let _ = shaped.paint(point(text_bounds.origin.x, y), line_height, window, cx);
                for selection in selections.iter() {
                    let head = selection.head;
                    if head < line_start || head > line_end {
                        continue;
                    }
                    let x = shaped.x_for_index(offsets[head - line_start]);
                    window.paint_quad(fill(
                        Bounds::new(
                            point(text_bounds.origin.x + x, y),
                            size(px(2.0), line_height),
                        ),
                        paint.cursor,
                    ));
                }
                if paint.line_numbers {
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
                        format!(
                            "{} abierto en modo archivo grande (solo lectura, sin resaltado)",
                            path.display()
                        ),
                    );
                }
                Err(error) => self.notify_user(
                    NotificationLevel::Error,
                    format!("No se pudo mapear {}: {error}", path.display()),
                ),
            }
        } else {
            let file = match LoadedFile::read(&path) {
                Ok(file) => file,
                Err(error) => {
                    self.notify_user(
                        NotificationLevel::Error,
                        format!("No se pudo abrir {}: {error}", path.display()),
                    );
                    return;
                }
            };
            if file.lossy {
                self.notify_user(
                    NotificationLevel::Warning,
                    format!("{} contiene bytes no decodificables", path.display()),
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
                    format!(
                        "{} recuperado del journal; guarda para conservar los cambios",
                        path.display()
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
                    "Archivo cargado en memoria; ya se puede editar",
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
                format!("Sin journal para {}: {error}", path.display()),
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
        match &editor.file {
            Some(file) => {
                let text = editor.buffer.text();
                match file.write(&text) {
                    Ok(()) => {
                        editor.buffer.mark_saved();
                        editor.recovered = false;
                        let path = file.path.display().to_string();
                        self.notify_user(NotificationLevel::Info, format!("Guardado {path}"));
                    }
                    Err(error) => {
                        self.notify_user(
                            NotificationLevel::Error,
                            format!("No se pudo guardar: {error}"),
                        );
                    }
                }
                cx.notify();
            }
            None => self.save_active_as(cx),
        }
    }

    fn save_active_as(&mut self, cx: &mut Context<Self>) {
        let tab_id = self.active_tab().id;
        let receiver = cx.prompt_for_new_path(&self.factory.cwd, Some("sin-titulo.txt"));
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
            prompt: Some("Abrir archivo".into()),
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
                        "Archivo grande en solo lectura: usa editor.materialize para editarlo",
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
            "backspace" => editor.buffer.delete(1, 0).map(Some),
            "delete" => editor.buffer.delete(0, 1).map(Some),
            "enter" => {
                // Inherit the indentation of the line the cursor is on.
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
                editor
                    .buffer
                    .insert(&format!("\n{indent}"), false)
                    .map(Some)
            }
            "tab" if modifiers.shift => Ok(None),
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
            _ if !modifiers.control && !modifiers.alt => match key_char {
                Some(text) if !text.is_empty() => editor.buffer.insert(text, true).map(Some),
                _ => return,
            },
            _ => return,
        };
        if let Err(error) = result {
            self.notify_user(
                NotificationLevel::Error,
                format!("Edición fallida: {error}"),
            );
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
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn editor_char_at(&self, position: Point<Pixels>, window: &Window) -> Option<usize> {
        let editor = self
            .active_tab()
            .editor()
            .filter(|editor| !editor.is_large())?;
        let bounds = editor.last_text_bounds?;
        let metrics = self.factory.metrics;
        let row = ((f32::from(position.y - bounds.origin.y)) / metrics.height)
            .floor()
            .max(0.0) as usize;
        let line = (editor.scroll_line + row).min(editor.buffer.len_lines().saturating_sub(1));
        let text = editor.buffer.line(line).unwrap_or_default();
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
        let x = (position.x - bounds.origin.x).max(px(0.0));
        let byte = shaped.closest_index_for_x(x);
        let column = offsets
            .partition_point(|offset| *offset < byte)
            .min(text.chars().count());
        let column = if column > 0 && offsets[column] > byte {
            column - 1
        } else {
            column
        };
        Some(editor.buffer.char_at(Position { line, column }))
    }

    pub fn editor_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
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
        }
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
        let x = shaped.x_for_index(offsets[position.column.min(offsets.len() - 1)]);
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
