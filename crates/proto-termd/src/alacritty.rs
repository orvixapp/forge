//! `alacritty_terminal` behind the same `VtEngine` contract as Ghostty, for
//! parser and scrollback comparisons (`--engine alacritty`). It covers what
//! the benchmarks and the GUI need; Ghostty stays the product engine, so
//! shell-integration marks and Kitty keyboard encoding are absent here.

use alacritty_terminal::{
    event::{Event, EventListener},
    grid::{Dimensions, Scroll},
    index::{Column, Line},
    term::{ClipboardType, Config, Term, TermDamage, TermMode, cell::Flags, test::TermSize},
    vte::ansi::{Color, CursorShape, NamedColor, Processor, Rgb as AlacrittyRgb},
};
use anyhow::Result;
use proto_ghostty_vt::{
    CellStyle, CursorStyle, DirtyState, RenderCell, RenderCursor, RenderRow, RenderSnapshot, Rgb,
    ScrollViewport, SessionState,
};
use proto_ipc::{ClipboardTarget, KeyAction, KeyEvent, MouseEvent, TerminalKey, Viewport};
use std::sync::{Arc, Mutex};

/// Events the terminal emits while parsing, drained after each write.
#[derive(Default)]
struct Outbox {
    title: Option<String>,
    pty: Vec<u8>,
    clipboard: Vec<(ClipboardTarget, String)>,
}

#[derive(Clone, Default)]
struct Listener(Arc<Mutex<Outbox>>);

impl EventListener for Listener {
    fn send_event(&self, event: Event) {
        let mut outbox = self.0.lock().expect("outbox mutex poisoned");
        match event {
            Event::Title(title) => outbox.title = Some(title),
            Event::ResetTitle => outbox.title = Some(String::new()),
            Event::PtyWrite(text) => outbox.pty.extend_from_slice(text.as_bytes()),
            Event::ClipboardStore(kind, text) => outbox.clipboard.push((
                match kind {
                    ClipboardType::Clipboard => ClipboardTarget::Clipboard,
                    ClipboardType::Selection => ClipboardTarget::Primary,
                },
                text,
            )),
            _ => {}
        }
    }
}

pub struct AlacrittyEngine {
    term: Term<Listener>,
    parser: Processor,
    listener: Listener,
    title: String,
    write_pty: Box<dyn Fn(&[u8]) + Send>,
    clipboard: Box<dyn Fn(ClipboardTarget, String) + Send>,
    /// Rows the client has not seen since the last full frame.
    full_pending: bool,
}

impl AlacrittyEngine {
    pub fn new(
        cols: u16,
        rows: u16,
        scrollback_lines: usize,
        write_pty: impl Fn(&[u8]) + Send + 'static,
        clipboard: impl Fn(ClipboardTarget, String) + Send + 'static,
    ) -> Self {
        let listener = Listener::default();
        let config = Config {
            scrolling_history: scrollback_lines,
            ..Config::default()
        };
        let term = Term::new(
            config,
            &TermSize::new(usize::from(cols.max(1)), usize::from(rows.max(1))),
            listener.clone(),
        );
        Self {
            term,
            parser: Processor::new(),
            listener,
            title: String::new(),
            write_pty: Box::new(write_pty),
            clipboard: Box::new(clipboard),
            full_pending: true,
        }
    }

    fn drain_events(&mut self) {
        let outbox = std::mem::take(&mut *self.listener.0.lock().expect("outbox mutex poisoned"));
        if let Some(title) = outbox.title {
            self.title = title;
        }
        if !outbox.pty.is_empty() {
            (self.write_pty)(&outbox.pty);
        }
        for (target, text) in outbox.clipboard {
            (self.clipboard)(target, text);
        }
    }

    fn display_offset(&self) -> usize {
        self.term.grid().display_offset()
    }

    fn row(&self, y: usize) -> RenderRow {
        let grid = self.term.grid();
        let line = Line(i32::try_from(y).unwrap_or(i32::MAX) - i32::try_from(self.display_offset()).unwrap_or(0));
        let row = &grid[line];
        let colors = self.term.colors();
        let cells = (0..grid.columns())
            .map(|x| {
                let cell = &row[Column(x)];
                let spacer = cell
                    .flags
                    .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER);
                let mut text = String::new();
                if !spacer && cell.c != ' ' {
                    text.push(cell.c);
                    if let Some(zerowidth) = cell.zerowidth() {
                        text.extend(zerowidth);
                    }
                }
                let default_fg = matches!(cell.fg, Color::Named(NamedColor::Foreground));
                let default_bg = matches!(cell.bg, Color::Named(NamedColor::Background));
                RenderCell {
                    text,
                    foreground: (!default_fg).then(|| resolve_color(cell.fg, colors)),
                    background: (!default_bg).then(|| resolve_color(cell.bg, colors)),
                    styled: !cell.flags.is_empty(),
                    style: CellStyle {
                        bold: cell.flags.contains(Flags::BOLD),
                        italic: cell.flags.contains(Flags::ITALIC),
                        faint: cell.flags.contains(Flags::DIM),
                        blink: false,
                        inverse: cell.flags.contains(Flags::INVERSE),
                        invisible: cell.flags.contains(Flags::HIDDEN),
                        strikethrough: cell.flags.contains(Flags::STRIKEOUT),
                        overline: false,
                        underline: if cell.flags.contains(Flags::DOUBLE_UNDERLINE) {
                            2
                        } else {
                            u8::from(cell.flags.contains(Flags::UNDERLINE))
                        },
                    },
                    hyperlink: cell.hyperlink().map(|link| link.uri().to_owned()),
                }
            })
            .collect();
        RenderRow {
            y: u16::try_from(y).unwrap_or(u16::MAX),
            cells,
            prompt: 0,
        }
    }

    fn cursor(&self) -> Option<RenderCursor> {
        let content = self.term.renderable_content();
        let cursor = content.cursor;
        let y = usize::try_from(cursor.point.line.0).ok()?;
        let style = match cursor.shape {
            CursorShape::Block => CursorStyle::Block,
            CursorShape::Underline => CursorStyle::Underline,
            CursorShape::Beam => CursorStyle::Bar,
            CursorShape::HollowBlock => CursorStyle::HollowBlock,
            CursorShape::Hidden => return None,
        };
        Some(RenderCursor {
            x: u16::try_from(cursor.point.column.0).unwrap_or(u16::MAX),
            y: u16::try_from(y).unwrap_or(u16::MAX),
            visible: self.term.mode().contains(TermMode::SHOW_CURSOR),
            blinking: false,
            style,
        })
    }

    fn frame(&mut self, full: bool) -> RenderSnapshot {
        let rows = self.term.screen_lines();
        let cols = u16::try_from(self.term.columns()).unwrap_or(u16::MAX);
        let (dirty, lines): (DirtyState, Vec<usize>) = if full || self.full_pending {
            (DirtyState::Full, (0..rows).collect())
        } else {
            match self.term.damage() {
                TermDamage::Full => (DirtyState::Full, (0..rows).collect()),
                TermDamage::Partial(damage) => {
                    let lines: Vec<usize> = damage.map(|bounds| bounds.line).filter(|line| *line < rows).collect();
                    if lines.is_empty() {
                        (DirtyState::Clean, lines)
                    } else {
                        (DirtyState::Partial, lines)
                    }
                }
            }
        };
        self.full_pending = false;
        let dirty_rows = lines.into_iter().map(|y| self.row(y)).collect();
        self.term.reset_damage();
        RenderSnapshot {
            cols,
            rows: u16::try_from(rows).unwrap_or(u16::MAX),
            dirty,
            dirty_rows,
            cursor: self.cursor(),
        }
    }
}

impl crate::daemon::VtEngine for AlacrittyEngine {
    fn write(&mut self, data: &[u8]) {
        self.parser.advance(&mut self.term, data);
        self.drain_events();
    }

    fn resize(&mut self, cols: u16, rows: u16) -> Result<()> {
        self.term
            .resize(TermSize::new(usize::from(cols.max(1)), usize::from(rows.max(1))));
        self.full_pending = true;
        Ok(())
    }

    fn snapshot(&mut self) -> Result<RenderSnapshot> {
        Ok(self.frame(false))
    }

    fn full_snapshot(&mut self) -> Result<RenderSnapshot> {
        Ok(self.frame(true))
    }

    fn state(&self) -> Result<SessionState> {
        let mode = self.term.mode();
        Ok(SessionState {
            title: self.title.clone(),
            pwd: String::new(),
            mouse_tracking: mode.intersects(TermMode::MOUSE_MODE),
            alternate_screen: mode.contains(TermMode::ALT_SCREEN),
            bracketed_paste: mode.contains(TermMode::BRACKETED_PASTE),
        })
    }

    fn viewport(&self) -> Result<Viewport> {
        let screen = self.term.screen_lines() as u64;
        let total = self.term.grid().total_lines() as u64;
        let offset = total - screen - self.display_offset() as u64;
        Ok(Viewport {
            total,
            offset,
            len: screen,
        })
    }

    fn scroll(&mut self, scroll: ScrollViewport) {
        // Alacritty's delta is positive towards history; ours is negative.
        let scroll = match scroll {
            ScrollViewport::Top => Scroll::Top,
            ScrollViewport::Bottom => Scroll::Bottom,
            ScrollViewport::Delta(delta) => {
                Scroll::Delta(i32::try_from(-delta).unwrap_or(if delta < 0 { i32::MAX } else { i32::MIN }))
            }
            ScrollViewport::Row(row) => {
                let screen = self.term.screen_lines() as u64;
                let total = self.term.grid().total_lines() as u64;
                let target = total.saturating_sub(screen).saturating_sub(row);
                let current = self.display_offset() as u64;
                Scroll::Delta(i32::try_from(target as i64 - current as i64).unwrap_or(0))
            }
        };
        let before = self.display_offset();
        self.term.scroll_display(scroll);
        if self.display_offset() != before {
            self.full_pending = true;
        }
    }

    fn encode_key(&mut self, event: &KeyEvent) -> Result<Vec<u8>> {
        Ok(encode_key(event, self.term.mode().contains(TermMode::APP_CURSOR)))
    }

    fn encode_mouse(&mut self, _cols: u16, _rows: u16, event: MouseEvent) -> Result<Vec<u8>> {
        Ok(encode_mouse(event, self.term.mode()))
    }

    fn encode_paste(&mut self, text: &str) -> Result<Vec<u8>> {
        let mut bytes = Vec::with_capacity(text.len() + 12);
        let bracketed = self.term.mode().contains(TermMode::BRACKETED_PASTE);
        if bracketed {
            bytes.extend_from_slice(b"\x1b[200~");
        }
        bytes.extend_from_slice(text.as_bytes());
        if bracketed {
            bytes.extend_from_slice(b"\x1b[201~");
        }
        Ok(bytes)
    }

    fn text(&self) -> Result<String> {
        let grid = self.term.grid();
        let mut out = String::new();
        let first = -(i32::try_from(grid.history_size()).unwrap_or(0));
        let last = i32::try_from(grid.screen_lines()).unwrap_or(0);
        for line in first..last {
            let row = &grid[Line(line)];
            let mut text = String::new();
            for x in 0..grid.columns() {
                let cell = &row[Column(x)];
                if cell
                    .flags
                    .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
                {
                    continue;
                }
                text.push(cell.c);
            }
            out.push_str(text.trim_end());
            out.push('\n');
        }
        Ok(out)
    }

    fn prompt_row(&self, _from: u64, _backwards: bool) -> Result<Option<u64>> {
        // No OSC 133 tracking in alacritty_terminal.
        Ok(None)
    }
}

/// xterm 256-colour palette for cells without a configured override.
fn resolve_color(color: Color, colors: &alacritty_terminal::term::color::Colors) -> Rgb {
    let rgb = |c: AlacrittyRgb| Rgb {
        r: c.r,
        g: c.g,
        b: c.b,
    };
    match color {
        Color::Spec(c) => rgb(c),
        Color::Named(named) => colors[named].map_or_else(|| indexed(named as usize), rgb),
        Color::Indexed(index) => colors[usize::from(index)].map_or_else(|| indexed(usize::from(index)), rgb),
    }
}

fn indexed(index: usize) -> Rgb {
    const BASE: [(u8, u8, u8); 16] = [
        (0, 0, 0),
        (205, 0, 0),
        (0, 205, 0),
        (205, 205, 0),
        (0, 0, 238),
        (205, 0, 205),
        (0, 205, 205),
        (229, 229, 229),
        (127, 127, 127),
        (255, 0, 0),
        (0, 255, 0),
        (255, 255, 0),
        (92, 92, 255),
        (255, 0, 255),
        (0, 255, 255),
        (255, 255, 255),
    ];
    let (r, g, b) = match index {
        0..=15 => BASE[index],
        16..=231 => {
            let i = index - 16;
            let level = |v: usize| if v == 0 { 0 } else { u8::try_from(55 + v * 40).unwrap_or(255) };
            (level(i / 36), level((i / 6) % 6), level(i % 6))
        }
        232..=255 => {
            let v = u8::try_from(8 + (index - 232) * 10).unwrap_or(255);
            (v, v, v)
        }
        _ => (229, 229, 229),
    };
    Rgb { r, g, b }
}

/// Minimal xterm key encoder: text as typed, Ctrl+letter as control codes,
/// Alt as an ESC prefix, and CSI/SS3 sequences for the named keys.
fn encode_key(event: &KeyEvent, app_cursor: bool) -> Vec<u8> {
    if event.action == KeyAction::Release {
        return Vec::new();
    }
    let mods = event.mods;
    let mut out = Vec::new();
    if mods.alt {
        out.push(0x1b);
    }
    let modifier = 1
        + u8::from(mods.shift)
        + 2 * u8::from(mods.alt)
        + 4 * u8::from(mods.control);
    let csi = |code: &str, tilde: bool| -> Vec<u8> {
        if modifier > 1 {
            if tilde {
                format!("\x1b[{code};{modifier}~").into_bytes()
            } else {
                format!("\x1b[1;{modifier}{code}").into_bytes()
            }
        } else if tilde {
            format!("\x1b[{code}~").into_bytes()
        } else if app_cursor {
            format!("\x1bO{code}").into_bytes()
        } else {
            format!("\x1b[{code}").into_bytes()
        }
    };
    let bytes: Vec<u8> = match event.key {
        TerminalKey::Enter => b"\r".to_vec(),
        TerminalKey::Tab if mods.shift => b"\x1b[Z".to_vec(),
        TerminalKey::Tab => b"\t".to_vec(),
        TerminalKey::Backspace => b"\x7f".to_vec(),
        TerminalKey::Escape => b"\x1b".to_vec(),
        TerminalKey::ArrowUp => csi("A", false),
        TerminalKey::ArrowDown => csi("B", false),
        TerminalKey::ArrowRight => csi("C", false),
        TerminalKey::ArrowLeft => csi("D", false),
        TerminalKey::Home => csi("H", false),
        TerminalKey::End => csi("F", false),
        TerminalKey::Insert => csi("2", true),
        TerminalKey::Delete => csi("3", true),
        TerminalKey::PageUp => csi("5", true),
        TerminalKey::PageDown => csi("6", true),
        TerminalKey::F1 => b"\x1bOP".to_vec(),
        TerminalKey::F2 => b"\x1bOQ".to_vec(),
        TerminalKey::F3 => b"\x1bOR".to_vec(),
        TerminalKey::F4 => b"\x1bOS".to_vec(),
        TerminalKey::F5 => csi("15", true),
        TerminalKey::F6 => csi("17", true),
        TerminalKey::F7 => csi("18", true),
        TerminalKey::F8 => csi("19", true),
        TerminalKey::F9 => csi("20", true),
        TerminalKey::F10 => csi("21", true),
        TerminalKey::F11 => csi("23", true),
        TerminalKey::F12 => csi("24", true),
        _ => {
            if mods.control
                && let Some(text) = &event.text
                && let Some(c) = text.chars().next()
                && c.is_ascii_alphabetic()
            {
                vec![c.to_ascii_uppercase() as u8 & 0x1f]
            } else if mods.control && event.unshifted_codepoint == u32::from(' ') {
                vec![0]
            } else {
                event.text.clone().unwrap_or_default().into_bytes()
            }
        }
    };
    out.extend(bytes);
    out
}

/// SGR (1006) mouse reports when the application asked for any mouse mode.
fn encode_mouse(event: MouseEvent, mode: &TermMode) -> Vec<u8> {
    use proto_ipc::{MouseAction, MouseButton};
    if !mode.intersects(TermMode::MOUSE_MODE) {
        return Vec::new();
    }
    let Some(button) = event.button else {
        return Vec::new();
    };
    let mut code = match button {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
        MouseButton::WheelUp => 64,
        MouseButton::WheelDown => 65,
        MouseButton::WheelLeft => 66,
        MouseButton::WheelRight => 67,
        MouseButton::Back => 128,
        MouseButton::Forward => 129,
    };
    if event.mods.shift {
        code += 4;
    }
    if event.mods.alt {
        code += 8;
    }
    if event.mods.control {
        code += 16;
    }
    if event.action == MouseAction::Motion {
        code += 32;
    }
    let release = event.action == MouseAction::Release;
    format!(
        "\x1b[<{code};{};{}{}",
        event.col + 1,
        event.row + 1,
        if release { 'm' } else { 'M' }
    )
    .into_bytes()
}
