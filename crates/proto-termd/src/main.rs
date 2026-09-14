//! `forge-termd` prototype: PTY sessions behind Ghostty's VT engine, served
//! over a Unix socket (Linux/macOS) or a named pipe (Windows, `ConPTY` via
//! `portable-pty`).

#[cfg(not(any(unix, windows)))]
fn main() {
    eprintln!("proto-termd supports Unix sockets and Windows named pipes only");
    std::process::exit(2);
}

#[cfg(any(unix, windows))]
mod search;

#[cfg(any(unix, windows))]
mod transport;

#[cfg(any(unix, windows))]
mod daemon {
    use anyhow::{Context, Result, bail};
    use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
    use proto_ghostty_vt::{
        ClipboardLocation, DirtyState, GhosttyLibrary, GhosttyTerminal, KeyEncoder, KeyInput,
        MouseEncoder, MouseInput, RenderSnapshot, ScrollViewport, SessionState,
    };
    use proto_ipc::{
        CellStyle, ClientMessage, ClipboardTarget, CursorStyle, FrameKind, KeyAction, KeyEvent,
        KeyMods, MouseAction, MouseButton, MouseEvent, PROTOCOL_VERSION, ProcessSignal,
        PromptDirection, Rgb, ScreenCell, ScreenCursor, ScreenRow, ScrollRequest, SearchMatch,
        ServerMessage, TerminalKey, Viewport, read_message, write_message,
    };
    use std::{
        collections::{HashMap, VecDeque},
        io::{Read, Write},
        path::PathBuf,
        sync::{Arc, Mutex, mpsc},
        thread,
        time::{SystemTime, UNIX_EPOCH},
    };
    use tokio::{
        io::{AsyncRead, AsyncWrite},
        sync::{RwLock, broadcast, mpsc as async_mpsc},
    };
    use tracing::{error, info, warn};

    const BACKLOG_LIMIT: usize = 4 * 1024 * 1024;
    const CONNECTION_QUEUE: usize = 256;

    #[derive(Debug, Clone)]
    enum SessionEvent {
        Output(Vec<u8>),
        ScreenPatch {
            revision: u64,
            cols: u16,
            rows: u16,
            full: bool,
            dirty_rows: Vec<ScreenRow>,
            cursor: Option<ScreenCursor>,
            viewport: Viewport,
        },
        Info(SessionState),
        /// OSC 52 and friends; the client applies its clipboard policy.
        Clipboard {
            target: ClipboardTarget,
            text: String,
            program: String,
        },
        Exited(Option<u32>),
    }

    /// Terminal state-machine boundary. PTY/session code depends on this
    /// contract rather than on Ghostty, so another engine can be benchmarked
    /// without changing lifecycle, IPC or rendering code.
    trait VtEngine: Send {
        fn write(&mut self, data: &[u8]);
        fn resize(&mut self, cols: u16, rows: u16) -> Result<()>;
        fn snapshot(&mut self) -> Result<RenderSnapshot>;
        fn full_snapshot(&mut self) -> Result<RenderSnapshot>;
        fn state(&self) -> Result<SessionState>;
        fn viewport(&self) -> Result<Viewport>;
        fn scroll(&mut self, scroll: ScrollViewport);
        fn encode_key(&mut self, event: &KeyEvent) -> Result<Vec<u8>>;
        fn encode_mouse(&mut self, cols: u16, rows: u16, event: MouseEvent) -> Result<Vec<u8>>;
        fn encode_paste(&mut self, text: &str) -> Result<Vec<u8>>;
        /// Plain text of the whole scrollable area, one line per row, for
        /// search. Must not disturb render state or the viewport.
        fn text(&self) -> Result<String>;
        /// Nearest OSC 133 prompt row before/after `from`.
        fn prompt_row(&self, from: u64, backwards: bool) -> Result<Option<u64>>;
    }

    struct GhosttyVtEngine {
        terminal: GhosttyTerminal,
        key_encoder: KeyEncoder,
        mouse_encoder: MouseEncoder,
    }

    impl VtEngine for GhosttyVtEngine {
        fn write(&mut self, data: &[u8]) {
            self.terminal.write(data);
        }

        fn resize(&mut self, cols: u16, rows: u16) -> Result<()> {
            self.terminal
                .resize(cols, rows)
                .context("resize Ghostty terminal")
        }

        fn snapshot(&mut self) -> Result<RenderSnapshot> {
            self.terminal
                .render_snapshot()
                .context("update Ghostty render state")
        }

        fn full_snapshot(&mut self) -> Result<RenderSnapshot> {
            self.terminal.full_snapshot().context("read full frame")
        }

        fn state(&self) -> Result<SessionState> {
            self.terminal.session_state().context("read session state")
        }

        fn viewport(&self) -> Result<Viewport> {
            let scrollbar = self.terminal.scrollbar().context("read scrollbar")?;
            Ok(Viewport {
                total: scrollbar.total,
                offset: scrollbar.offset,
                len: scrollbar.len,
            })
        }

        fn scroll(&mut self, scroll: ScrollViewport) {
            self.terminal.scroll_viewport(scroll);
        }

        fn encode_key(&mut self, event: &KeyEvent) -> Result<Vec<u8>> {
            self.key_encoder
                .encode(&self.terminal, &key_input(event))
                .context("encode key")
        }

        fn encode_mouse(&mut self, cols: u16, rows: u16, event: MouseEvent) -> Result<Vec<u8>> {
            self.mouse_encoder
                .encode(&self.terminal, cols, rows, mouse_input(event))
                .context("encode mouse event")
        }

        fn encode_paste(&mut self, text: &str) -> Result<Vec<u8>> {
            self.terminal.encode_paste(text).context("encode paste")
        }

        fn text(&self) -> Result<String> {
            self.terminal
                .snapshot_text()
                .context("format scrollback as text")
        }

        fn prompt_row(&self, from: u64, backwards: bool) -> Result<Option<u64>> {
            self.terminal
                .prompt_row(from, backwards)
                .context("scan prompt rows")
        }
    }

    struct Session {
        input: mpsc::SyncSender<Vec<u8>>,
        master: Mutex<Box<dyn MasterPty + Send>>,
        child: Mutex<Box<dyn Child + Send + Sync>>,
        backlog: Mutex<VecDeque<u8>>,
        terminal: Mutex<Box<dyn VtEngine>>,
        cols: std::sync::atomic::AtomicU16,
        rows: std::sync::atomic::AtomicU16,
        revision: std::sync::atomic::AtomicU64,
        exit_code: Mutex<Option<u32>>,
        events: broadcast::Sender<SessionEvent>,
        /// What the clients last saw, so cursor moves and viewport changes
        /// without dirty rows still produce a patch.
        last_frame: Mutex<(Option<ScreenCursor>, Viewport)>,
        last_state: Mutex<SessionState>,
    }

    impl Session {
        fn snapshot(&self) -> Vec<u8> {
            self.backlog
                .lock()
                .expect("backlog mutex poisoned")
                .iter()
                .copied()
                .collect()
        }

        fn resize(&self, cols: u16, rows: u16) -> Result<()> {
            self.master
                .lock()
                .expect("master mutex poisoned")
                .resize(PtySize {
                    rows,
                    cols,
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .context("resize PTY")?;
            self.cols
                .store(cols.max(1), std::sync::atomic::Ordering::Relaxed);
            self.rows
                .store(rows.max(1), std::sync::atomic::Ordering::Relaxed);
            self.terminal
                .lock()
                .expect("terminal mutex poisoned")
                .resize(cols, rows)?;
            self.emit_screen()
        }

        fn shutdown(&self) -> Result<()> {
            self.child
                .lock()
                .expect("child mutex poisoned")
                .kill()
                .context("kill PTY child")
        }

        /// Signals the whole process group of the shell (it is a session
        /// leader, so the group id is its pid), like the driver does for
        /// Ctrl+C but for any signal and regardless of terminal modes.
        #[cfg(unix)]
        fn signal(&self, signal: ProcessSignal) -> Result<()> {
            let pid = self
                .child
                .lock()
                .expect("child mutex poisoned")
                .process_id()
                .context("the shell already exited")?;
            let pid = rustix::process::Pid::from_raw(i32::try_from(pid).context("pid range")?)
                .context("invalid pid")?;
            let signal = match signal {
                ProcessSignal::Interrupt => rustix::process::Signal::INT,
                ProcessSignal::Terminate => rustix::process::Signal::TERM,
                ProcessSignal::Kill => rustix::process::Signal::KILL,
                ProcessSignal::Hangup => rustix::process::Signal::HUP,
            };
            rustix::process::kill_process_group(pid, signal).context("send signal")
        }

        /// `ConPTY` has no signals: an interrupt is the Ctrl+C byte, and
        /// everything else terminates the console process.
        #[cfg(windows)]
        fn signal(&self, signal: ProcessSignal) -> Result<()> {
            match signal {
                ProcessSignal::Interrupt => self
                    .input
                    .send(vec![0x03])
                    .context("PTY input queue closed"),
                ProcessSignal::Terminate | ProcessSignal::Kill | ProcessSignal::Hangup => {
                    self.shutdown()
                }
            }
        }

        /// Scrolls the viewport to the previous/next prompt line marked by
        /// OSC 133 (shell integration); nothing happens without marks.
        fn scroll_to_prompt(&self, direction: PromptDirection) -> Result<bool> {
            let target = {
                let mut terminal = self.terminal.lock().expect("terminal mutex poisoned");
                let viewport = terminal.viewport()?;
                let target = match direction {
                    PromptDirection::Previous => terminal.prompt_row(viewport.offset, true)?,
                    PromptDirection::Next => terminal.prompt_row(viewport.offset, false)?,
                };
                if let Some(row) = target {
                    terminal.scroll(ScrollViewport::Row(row));
                }
                target
            };
            self.emit_screen()?;
            Ok(target.is_some())
        }

        fn feed_vt(&self, data: &[u8]) -> Result<()> {
            self.terminal
                .lock()
                .expect("terminal mutex poisoned")
                .write(data);
            self.emit_screen()
        }

        fn emit_screen(&self) -> Result<()> {
            let (frame, viewport, state) = {
                let mut terminal = self.terminal.lock().expect("terminal mutex poisoned");
                let frame = terminal.snapshot()?;
                let viewport = terminal.viewport()?;
                let state = terminal.state()?;
                (frame, viewport, state)
            };
            let cursor = frame.cursor.map(convert_cursor);
            let changed = {
                let mut last = self.last_frame.lock().expect("last frame mutex poisoned");
                let changed = frame.dirty != DirtyState::Clean || *last != (cursor, viewport);
                *last = (cursor, viewport);
                changed
            };
            if changed {
                self.publish_frame(frame, cursor, viewport);
            }
            let mut last_state = self.last_state.lock().expect("state mutex poisoned");
            if *last_state != state {
                *last_state = state.clone();
                let _ = self.events.send(SessionEvent::Info(state));
            }
            Ok(())
        }

        fn publish_frame(
            &self,
            frame: RenderSnapshot,
            cursor: Option<ScreenCursor>,
            viewport: Viewport,
        ) {
            let revision = self
                .revision
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                + 1;
            let _ = self.events.send(SessionEvent::ScreenPatch {
                revision,
                cols: frame.cols,
                rows: frame.rows,
                full: frame.dirty == DirtyState::Full,
                dirty_rows: frame.dirty_rows.into_iter().map(convert_row).collect(),
                cursor,
                viewport,
            });
        }

        /// Everything a client that attaches mid-session needs to draw.
        fn full_frame(&self) -> Result<(SessionEvent, SessionState)> {
            let mut terminal = self.terminal.lock().expect("terminal mutex poisoned");
            let frame = terminal.full_snapshot()?;
            let viewport = terminal.viewport()?;
            let state = terminal.state()?;
            let cursor = frame.cursor.map(convert_cursor);
            *self.last_frame.lock().expect("last frame mutex poisoned") = (cursor, viewport);
            let revision = self
                .revision
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                + 1;
            Ok((
                SessionEvent::ScreenPatch {
                    revision,
                    cols: frame.cols,
                    rows: frame.rows,
                    full: true,
                    dirty_rows: frame.dirty_rows.into_iter().map(convert_row).collect(),
                    cursor,
                    viewport,
                },
                state,
            ))
        }

        /// Encodes a key with the terminal's current modes and writes it to
        /// the PTY. Typing while scrolled back jumps to the live screen.
        fn key(&self, event: &KeyEvent) -> Result<()> {
            let bytes = {
                let mut terminal = self.terminal.lock().expect("terminal mutex poisoned");
                let bytes = terminal.encode_key(event)?;
                if event.action != KeyAction::Release && !bytes.is_empty() {
                    terminal.scroll(ScrollViewport::Bottom);
                }
                bytes
            };
            if !bytes.is_empty() {
                self.input.send(bytes).context("PTY input queue closed")?;
                self.emit_screen()?;
            }
            Ok(())
        }

        fn mouse(&self, event: MouseEvent) -> Result<()> {
            let bytes = {
                let mut terminal = self.terminal.lock().expect("terminal mutex poisoned");
                let cols = self.cols.load(std::sync::atomic::Ordering::Relaxed);
                let rows = self.rows.load(std::sync::atomic::Ordering::Relaxed);
                terminal.encode_mouse(cols, rows, event)?
            };
            if !bytes.is_empty() {
                self.input.send(bytes).context("PTY input queue closed")?;
            }
            Ok(())
        }

        fn paste(&self, text: &str) -> Result<()> {
            let bytes = {
                let mut terminal = self.terminal.lock().expect("terminal mutex poisoned");
                terminal.scroll(ScrollViewport::Bottom);
                terminal.encode_paste(text)?
            };
            self.input.send(bytes).context("PTY input queue closed")?;
            self.emit_screen()
        }

        fn scroll(&self, scroll: ScrollRequest) -> Result<()> {
            self.terminal
                .lock()
                .expect("terminal mutex poisoned")
                .scroll(match scroll {
                    ScrollRequest::Top => ScrollViewport::Top,
                    ScrollRequest::Bottom => ScrollViewport::Bottom,
                    ScrollRequest::Delta(delta) => ScrollViewport::Delta(delta),
                    ScrollRequest::Row(row) => ScrollViewport::Row(row),
                });
            self.emit_screen()
        }

        /// Formats the scrollback once and matches in Rust; the terminal
        /// lock is held only while Ghostty dumps the text.
        fn search(
            &self,
            query: &str,
            use_regex: bool,
            case_sensitive: bool,
        ) -> Result<Vec<SearchMatch>> {
            let text = self
                .terminal
                .lock()
                .expect("terminal mutex poisoned")
                .text()?;
            crate::search::search_text(&text, query, use_regex, case_sensitive)
        }
    }

    fn convert_cursor(cursor: proto_ghostty_vt::RenderCursor) -> ScreenCursor {
        ScreenCursor {
            x: cursor.x,
            y: cursor.y,
            visible: cursor.visible,
            blinking: cursor.blinking,
            style: match cursor.style {
                proto_ghostty_vt::CursorStyle::Bar => CursorStyle::Bar,
                proto_ghostty_vt::CursorStyle::Block => CursorStyle::Block,
                proto_ghostty_vt::CursorStyle::Underline => CursorStyle::Underline,
                proto_ghostty_vt::CursorStyle::HollowBlock => CursorStyle::HollowBlock,
            },
        }
    }

    fn convert_row(row: proto_ghostty_vt::RenderRow) -> ScreenRow {
        ScreenRow {
            y: row.y,
            prompt: row.prompt,
            cells: row
                .cells
                .into_iter()
                .map(|cell| ScreenCell {
                    hyperlink: cell.hyperlink,
                    text: cell.text,
                    foreground: cell.foreground.map(|color| Rgb {
                        r: color.r,
                        g: color.g,
                        b: color.b,
                    }),
                    background: cell.background.map(|color| Rgb {
                        r: color.r,
                        g: color.g,
                        b: color.b,
                    }),
                    styled: cell.styled,
                    style: CellStyle {
                        bold: cell.style.bold,
                        italic: cell.style.italic,
                        faint: cell.style.faint,
                        blink: cell.style.blink,
                        inverse: cell.style.inverse,
                        invisible: cell.style.invisible,
                        strikethrough: cell.style.strikethrough,
                        overline: cell.style.overline,
                        underline: cell.style.underline,
                    },
                })
                .collect(),
        }
    }

    fn session_info(session_id: u64, state: &SessionState) -> ServerMessage {
        ServerMessage::SessionInfo {
            session_id,
            title: state.title.clone(),
            pwd: decode_pwd(&state.pwd),
            mouse_tracking: state.mouse_tracking,
            alternate_screen: state.alternate_screen,
            bracketed_paste: state.bracketed_paste,
        }
    }

    /// OSC 7 carries a `file://host/path` URI; OSC 9/1337 a bare path.
    fn decode_pwd(raw: &str) -> Option<String> {
        if raw.is_empty() {
            return None;
        }
        let path = raw.strip_prefix("file://").map_or(raw, |rest| {
            rest.find('/').map_or("", |slash| &rest[slash..])
        });
        (!path.is_empty()).then(|| percent_decode(path))
    }

    fn percent_decode(text: &str) -> String {
        let bytes = text.as_bytes();
        let mut out = Vec::with_capacity(bytes.len());
        let mut index = 0;
        while index < bytes.len() {
            if bytes[index] == b'%'
                && index + 2 < bytes.len()
                && let Ok(value) = u8::from_str_radix(&text[index + 1..index + 3], 16)
            {
                out.push(value);
                index += 3;
            } else {
                out.push(bytes[index]);
                index += 1;
            }
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    fn mods_bits(mods: KeyMods) -> u16 {
        u16::from(mods.shift)
            | (u16::from(mods.control) << 1)
            | (u16::from(mods.alt) << 2)
            | (u16::from(mods.super_key) << 3)
    }

    fn key_input(event: &KeyEvent) -> KeyInput {
        KeyInput {
            action: match event.action {
                KeyAction::Release => 0,
                KeyAction::Press => 1,
                KeyAction::Repeat => 2,
            },
            key: ghostty_key(event.key),
            mods: mods_bits(event.mods),
            text: event.text.clone(),
            unshifted_codepoint: event.unshifted_codepoint,
        }
    }

    fn mouse_input(event: MouseEvent) -> MouseInput {
        MouseInput {
            action: match event.action {
                MouseAction::Press => 0,
                MouseAction::Release => 1,
                MouseAction::Motion => 2,
            },
            button: event.button.map(|button| match button {
                MouseButton::Left => 1,
                MouseButton::Right => 2,
                MouseButton::Middle => 3,
                MouseButton::WheelUp => 4,
                MouseButton::WheelDown => 5,
                MouseButton::WheelLeft => 6,
                MouseButton::WheelRight => 7,
                MouseButton::Back => 8,
                MouseButton::Forward => 9,
            }),
            mods: mods_bits(event.mods),
            col: event.col,
            row: event.row,
        }
    }

    /// `GHOSTTY_KEY_*` codes from `include/ghostty/vt/key/event.h` of the
    /// pinned commit; the enum is dense so the discriminant is the index.
    #[allow(clippy::too_many_lines)]
    fn ghostty_key(key: TerminalKey) -> i32 {
        use TerminalKey as K;
        match key {
            K::Unidentified => 0,
            K::Backquote => 1,
            K::Backslash => 2,
            K::BracketLeft => 3,
            K::BracketRight => 4,
            K::Comma => 5,
            K::Digit0 => 6,
            K::Digit1 => 7,
            K::Digit2 => 8,
            K::Digit3 => 9,
            K::Digit4 => 10,
            K::Digit5 => 11,
            K::Digit6 => 12,
            K::Digit7 => 13,
            K::Digit8 => 14,
            K::Digit9 => 15,
            K::Equal => 16,
            K::IntlBackslash => 17,
            K::A => 20,
            K::B => 21,
            K::C => 22,
            K::D => 23,
            K::E => 24,
            K::F => 25,
            K::G => 26,
            K::H => 27,
            K::I => 28,
            K::J => 29,
            K::K => 30,
            K::L => 31,
            K::M => 32,
            K::N => 33,
            K::O => 34,
            K::P => 35,
            K::Q => 36,
            K::R => 37,
            K::S => 38,
            K::T => 39,
            K::U => 40,
            K::V => 41,
            K::W => 42,
            K::X => 43,
            K::Y => 44,
            K::Z => 45,
            K::Minus => 46,
            K::Period => 47,
            K::Quote => 48,
            K::Semicolon => 49,
            K::Slash => 50,
            K::AltLeft => 51,
            K::AltRight => 52,
            K::Backspace => 53,
            K::CapsLock => 54,
            K::ContextMenu => 55,
            K::ControlLeft => 56,
            K::ControlRight => 57,
            K::Enter => 58,
            K::MetaLeft => 59,
            K::MetaRight => 60,
            K::ShiftLeft => 61,
            K::ShiftRight => 62,
            K::Space => 63,
            K::Tab => 64,
            K::Delete => 68,
            K::End => 69,
            K::Home => 71,
            K::Insert => 72,
            K::PageDown => 73,
            K::PageUp => 74,
            K::ArrowDown => 75,
            K::ArrowLeft => 76,
            K::ArrowRight => 77,
            K::ArrowUp => 78,
            K::NumLock => 79,
            K::Numpad0 => 80,
            K::Numpad1 => 81,
            K::Numpad2 => 82,
            K::Numpad3 => 83,
            K::Numpad4 => 84,
            K::Numpad5 => 85,
            K::Numpad6 => 86,
            K::Numpad7 => 87,
            K::Numpad8 => 88,
            K::Numpad9 => 89,
            K::NumpadAdd => 90,
            K::NumpadDecimal => 95,
            K::NumpadDivide => 96,
            K::NumpadEnter => 97,
            K::NumpadEqual => 98,
            K::NumpadMultiply => 104,
            K::NumpadSubtract => 107,
            K::Escape => 120,
            K::F1 => 121,
            K::F2 => 122,
            K::F3 => 123,
            K::F4 => 124,
            K::F5 => 125,
            K::F6 => 126,
            K::F7 => 127,
            K::F8 => 128,
            K::F9 => 129,
            K::F10 => 130,
            K::F11 => 131,
            K::F12 => 132,
            K::F13 => 133,
            K::F14 => 134,
            K::F15 => 135,
            K::F16 => 136,
            K::F17 => 137,
            K::F18 => 138,
            K::F19 => 139,
            K::F20 => 140,
            K::F21 => 141,
            K::F22 => 142,
            K::F23 => 143,
            K::F24 => 144,
            K::PrintScreen => 148,
            K::ScrollLock => 149,
            K::Pause => 150,
        }
    }

    struct Daemon {
        instance_id: u64,
        ghostty: GhosttyLibrary,
        sessions: RwLock<HashMap<u64, Arc<Session>>>,
        next_session_id: std::sync::atomic::AtomicU64,
    }

    impl Daemon {
        fn new(ghostty: GhosttyLibrary) -> Self {
            let started = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default();
            let started = started.as_secs() ^ (u64::from(started.subsec_nanos()) << 32);
            Self {
                instance_id: started ^ u64::from(std::process::id()),
                ghostty,
                sessions: RwLock::new(HashMap::new()),
                next_session_id: std::sync::atomic::AtomicU64::new(0),
            }
        }

        async fn create_session(
            &self,
            command: String,
            args: Vec<String>,
            cwd: PathBuf,
            cols: u16,
            rows: u16,
            env: Vec<(String, String)>,
        ) -> Result<u64> {
            let pair = native_pty_system()
                .openpty(PtySize {
                    rows,
                    cols,
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .context("open PTY")?;
            let mut builder = CommandBuilder::new(command);
            builder.cwd(cwd);
            for arg in args {
                builder.arg(arg);
            }
            for (key, value) in env {
                builder.env(key, value);
            }
            let child = pair.slave.spawn_command(builder).context("spawn command")?;
            drop(pair.slave);
            let reader = pair.master.try_clone_reader().context("clone PTY reader")?;
            let writer = pair.master.take_writer().context("take PTY writer")?;
            let (input_tx, input_rx) = mpsc::sync_channel::<Vec<u8>>(128);
            let (events, _) = broadcast::channel(256);
            let mut terminal = self
                .ghostty
                .terminal(cols, rows)
                .context("create Ghostty terminal")?;
            terminal
                .set_scrollback_max_bytes(64 * 1024 * 1024)
                .context("configure terminal scrollback bytes")?;
            terminal
                .set_scrollback_max_lines(100_000)
                .context("configure terminal scrollback lines")?;
            // Replies to the application's queries (DA, DSR, mode reports)
            // go straight back to the PTY.
            let replies = input_tx.clone();
            terminal
                .set_write_pty(move |bytes| {
                    let _ = replies.send(bytes.to_vec());
                })
                .context("install PTY reply callback")?;
            // Clipboard writes are forwarded to every attached client; the
            // daemon has no clipboard and never decides for the user.
            let clipboard_events = events.clone();
            terminal
                .set_clipboard_write(move |location, text, program| {
                    let target = match location {
                        ClipboardLocation::Standard | ClipboardLocation::Selection => {
                            ClipboardTarget::Clipboard
                        }
                        ClipboardLocation::Primary => ClipboardTarget::Primary,
                    };
                    let _ = clipboard_events.send(SessionEvent::Clipboard {
                        target,
                        text,
                        program,
                    });
                })
                .context("install clipboard write callback")?;
            let key_encoder = self.ghostty.key_encoder().context("create key encoder")?;
            let mouse_encoder = self
                .ghostty
                .mouse_encoder()
                .context("create mouse encoder")?;
            let engine = GhosttyVtEngine {
                terminal,
                key_encoder,
                mouse_encoder,
            };
            let session = Arc::new(Session {
                input: input_tx,
                master: Mutex::new(pair.master),
                child: Mutex::new(child),
                backlog: Mutex::new(VecDeque::with_capacity(64 * 1024)),
                terminal: Mutex::new(Box::new(engine)),
                cols: std::sync::atomic::AtomicU16::new(cols.max(1)),
                rows: std::sync::atomic::AtomicU16::new(rows.max(1)),
                revision: std::sync::atomic::AtomicU64::new(0),
                exit_code: Mutex::new(None),
                events,
                last_frame: Mutex::new((None, Viewport::default())),
                last_state: Mutex::new(SessionState::default()),
            });
            let session_id = self
                .next_session_id
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                + 1;

            let (reader_done_tx, reader_done_rx) = mpsc::sync_channel(1);
            spawn_io_threads(
                session_id,
                Arc::clone(&session),
                reader,
                writer,
                input_rx,
                reader_done_tx,
            )?;
            spawn_wait_thread(session_id, Arc::clone(&session), reader_done_rx)?;

            self.sessions.write().await.insert(session_id, session);
            info!(session_id, "created PTY session");
            Ok(session_id)
        }

        async fn session(&self, id: u64) -> Result<Arc<Session>> {
            self.sessions
                .read()
                .await
                .get(&id)
                .cloned()
                .with_context(|| format!("unknown session {id}"))
        }
    }

    fn spawn_io_threads(
        session_id: u64,
        read_session: Arc<Session>,
        mut reader: Box<dyn Read + Send>,
        mut writer: Box<dyn Write + Send>,
        input_rx: mpsc::Receiver<Vec<u8>>,
        reader_done_tx: mpsc::SyncSender<()>,
    ) -> Result<()> {
        thread::Builder::new()
            .name(format!("forge-pty-read-{session_id}"))
            .spawn(move || {
                // Larger reads reduce snapshot/event amplification during
                // sustained output while the dedicated writer thread keeps
                // keyboard input independent from VT parsing.
                let mut chunk = vec![0_u8; 64 * 1024];
                loop {
                    match reader.read(&mut chunk) {
                        Ok(0) => break,
                        Ok(count) => {
                            let data = chunk[..count].to_vec();
                            append_bounded(&read_session.backlog, &data);
                            if let Err(error) = read_session.feed_vt(&data) {
                                warn!(session_id, %error, "Ghostty VT update failed");
                            }
                            let _ = read_session.events.send(SessionEvent::Output(data));
                        }
                        Err(error) => {
                            warn!(session_id, %error, "PTY read failed");
                            break;
                        }
                    }
                }
                let _ = reader_done_tx.send(());
            })
            .context("spawn PTY reader thread")?;

        thread::Builder::new()
            .name(format!("forge-pty-write-{session_id}"))
            .spawn(move || {
                while let Ok(data) = input_rx.recv() {
                    if writer.write_all(&data).is_err() || writer.flush().is_err() {
                        break;
                    }
                }
            })
            .context("spawn PTY writer thread")?;
        Ok(())
    }

    fn spawn_wait_thread(
        session_id: u64,
        wait_session: Arc<Session>,
        reader_done_rx: mpsc::Receiver<()>,
    ) -> Result<()> {
        thread::Builder::new()
            .name(format!("forge-pty-wait-{session_id}"))
            .spawn(move || {
                let code = loop {
                    let status = wait_session
                        .child
                        .lock()
                        .expect("child mutex poisoned")
                        .try_wait();
                    match status {
                        Ok(Some(value)) => break Some(value.exit_code()),
                        Ok(None) => thread::sleep(std::time::Duration::from_millis(20)),
                        Err(error) => {
                            warn!(session_id, %error, "waiting for PTY child failed");
                            break None;
                        }
                    }
                };
                // Drain final bytes before publishing the terminal exit.
                let _ = reader_done_rx.recv_timeout(std::time::Duration::from_secs(1));
                if let Some(value) = code {
                    *wait_session
                        .exit_code
                        .lock()
                        .expect("exit code mutex poisoned") = Some(value);
                }
                let _ = wait_session.events.send(SessionEvent::Exited(code));
            })
            .context("spawn PTY wait thread")?;
        Ok(())
    }

    fn append_bounded(backlog: &Mutex<VecDeque<u8>>, data: &[u8]) {
        let mut backlog = backlog.lock().expect("backlog mutex poisoned");
        let overflow = backlog
            .len()
            .saturating_add(data.len())
            .saturating_sub(BACKLOG_LIMIT);
        let remove = overflow.min(backlog.len());
        backlog.drain(..remove);
        if data.len() > BACKLOG_LIMIT {
            backlog.extend(&data[data.len() - BACKLOG_LIMIT..]);
        } else {
            backlog.extend(data);
        }
    }

    pub async fn run() -> Result<()> {
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| "proto_termd=info".into()),
            )
            .init();
        let options = parse_options()?;
        let ghostty = GhosttyLibrary::load(&options.ghostty_lib).with_context(|| {
            format!(
                "load libghostty-vt from {}; run scripts/bootstrap-ghostty.sh first",
                options.ghostty_lib.display()
            )
        })?;
        let socket = options.socket;
        let mut listener = crate::transport::Listener::bind(&socket).await?;
        info!(path = %socket.display(), "terminal daemon listening");
        let daemon = Arc::new(Daemon::new(ghostty));
        loop {
            let stream = listener.accept().await?;
            let daemon = Arc::clone(&daemon);
            tokio::spawn(async move {
                if let Err(error) = handle_connection(stream, daemon).await {
                    warn!(%error, "client disconnected with error");
                }
            });
        }
    }

    struct Options {
        socket: PathBuf,
        ghostty_lib: PathBuf,
    }

    fn parse_options() -> Result<Options> {
        let mut socket = PathBuf::from("/tmp/forge-prototype.sock");
        let mut ghostty_lib = std::env::var_os("FORGE_GHOSTTY_LIB").map_or_else(
            || "target/ghostty/lib/libghostty-vt.so".into(),
            PathBuf::from,
        );
        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--socket" => socket = args.next().context("--socket requires a path")?.into(),
                "--ghostty-lib" => {
                    ghostty_lib = args.next().context("--ghostty-lib requires a path")?.into();
                }
                _ => bail!("usage: proto-termd [--socket PATH] [--ghostty-lib PATH]"),
            }
        }
        Ok(Options {
            socket,
            ghostty_lib,
        })
    }

    async fn handle_connection<S>(stream: S, daemon: Arc<Daemon>) -> Result<()>
    where
        S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
    {
        let (mut reader, mut writer) = tokio::io::split(stream);
        let (out_tx, mut out_rx) =
            async_mpsc::channel::<(FrameKind, ServerMessage)>(CONNECTION_QUEUE);
        let writer_task = tokio::spawn(async move {
            while let Some((kind, message)) = out_rx.recv().await {
                write_message(&mut writer, kind, &message).await?;
            }
            Ok::<_, proto_ipc::ProtocolError>(())
        });

        let (_, first) = read_message::<_, ClientMessage>(&mut reader).await?;
        match first {
            ClientMessage::Initialize {
                protocol_version, ..
            } if protocol_version == PROTOCOL_VERSION => {
                out_tx
                    .send((
                        FrameKind::Response,
                        ServerMessage::Initialized {
                            protocol_version: PROTOCOL_VERSION,
                            daemon_instance: daemon.instance_id,
                        },
                    ))
                    .await?;
            }
            ClientMessage::Initialize {
                protocol_version, ..
            } => {
                send_error(
                    &out_tx,
                    format!(
                        "protocol mismatch: client {protocol_version}, daemon {PROTOCOL_VERSION}"
                    ),
                )
                .await;
                return Ok(());
            }
            _ => {
                send_error(&out_tx, "initialize must be the first message".into()).await;
                return Ok(());
            }
        }

        while let Ok((_, message)) = read_message::<_, ClientMessage>(&mut reader).await {
            if let Err(error) = dispatch(message, &daemon, &out_tx).await {
                send_error(&out_tx, error.to_string()).await;
            }
        }
        drop(out_tx);
        writer_task.await??;
        Ok(())
    }

    async fn dispatch(
        message: ClientMessage,
        daemon: &Arc<Daemon>,
        out_tx: &async_mpsc::Sender<(FrameKind, ServerMessage)>,
    ) -> Result<()> {
        match message {
            ClientMessage::CreateSession {
                request_id,
                command,
                args,
                cwd,
                cols,
                rows,
                env,
            } => {
                let session_id = daemon
                    .create_session(command, args, cwd, cols, rows, env)
                    .await?;
                out_tx
                    .send((
                        FrameKind::Response,
                        ServerMessage::SessionCreated {
                            request_id,
                            session_id,
                        },
                    ))
                    .await?;
            }
            ClientMessage::Attach { session_id } => {
                let session = daemon.session(session_id).await?;
                attach_session(session_id, session, out_tx).await?;
            }
            ClientMessage::Input { session_id, data } => {
                daemon
                    .session(session_id)
                    .await?
                    .input
                    .send(data)
                    .context("PTY input queue closed")?;
            }
            ClientMessage::Resize {
                session_id,
                cols,
                rows,
            } => daemon.session(session_id).await?.resize(cols, rows)?,
            ClientMessage::ShutdownSession { session_id } => {
                daemon.session(session_id).await?.shutdown()?;
            }
            ClientMessage::Key { session_id, event } => {
                daemon.session(session_id).await?.key(&event)?;
            }
            ClientMessage::Mouse { session_id, event } => {
                daemon.session(session_id).await?.mouse(event)?;
            }
            ClientMessage::Paste { session_id, text } => {
                daemon.session(session_id).await?.paste(&text)?;
            }
            ClientMessage::Scroll { session_id, scroll } => {
                daemon.session(session_id).await?.scroll(scroll)?;
            }
            ClientMessage::Search {
                session_id,
                request_id,
                query,
                regex,
                case_sensitive,
            } => {
                search(
                    daemon,
                    out_tx,
                    session_id,
                    request_id,
                    query,
                    regex,
                    case_sensitive,
                )
                .await?;
            }
            ClientMessage::Signal { session_id, signal } => {
                daemon.session(session_id).await?.signal(signal)?;
            }
            ClientMessage::ScrollToPrompt {
                session_id,
                direction,
            } => {
                daemon
                    .session(session_id)
                    .await?
                    .scroll_to_prompt(direction)?;
            }
            ClientMessage::ListSessions => list_sessions(daemon, out_tx).await?,
            ClientMessage::Detach { .. } => {
                // Attach forwarding tasks end when this connection closes. Per-session
                // detach tokens arrive in the next protocol iteration.
            }
            ClientMessage::Initialize { .. } => bail!("connection is already initialized"),
        }
        Ok(())
    }

    /// A bad pattern is the user's typo, not a daemon failure: it travels
    /// inside the results so the search bar shows it.
    async fn search(
        daemon: &Arc<Daemon>,
        out_tx: &async_mpsc::Sender<(FrameKind, ServerMessage)>,
        session_id: u64,
        request_id: u64,
        query: String,
        regex: bool,
        case_sensitive: bool,
    ) -> Result<()> {
        let (matches, error) =
            match daemon
                .session(session_id)
                .await?
                .search(&query, regex, case_sensitive)
            {
                Ok(matches) => (matches, None),
                Err(error) => (Vec::new(), Some(format!("{error:#}"))),
            };
        out_tx
            .send((
                FrameKind::Response,
                ServerMessage::SearchResults {
                    session_id,
                    request_id,
                    query,
                    matches,
                    error,
                },
            ))
            .await?;
        Ok(())
    }

    async fn list_sessions(
        daemon: &Arc<Daemon>,
        out_tx: &async_mpsc::Sender<(FrameKind, ServerMessage)>,
    ) -> Result<()> {
        let sessions = daemon.sessions.read().await;
        let mut summaries = Vec::with_capacity(sessions.len());
        for (id, session) in sessions.iter() {
            let state = session
                .last_state
                .lock()
                .expect("state mutex poisoned")
                .clone();
            summaries.push(proto_ipc::SessionSummary {
                session_id: *id,
                title: state.title,
                pwd: decode_pwd(&state.pwd),
                alive: session
                    .exit_code
                    .lock()
                    .expect("exit code mutex poisoned")
                    .is_none(),
            });
        }
        summaries.sort_by_key(|summary| summary.session_id);
        out_tx
            .send((
                FrameKind::Response,
                ServerMessage::Sessions {
                    sessions: summaries,
                },
            ))
            .await?;
        Ok(())
    }

    async fn attach_session(
        session_id: u64,
        session: Arc<Session>,
        out_tx: &async_mpsc::Sender<(FrameKind, ServerMessage)>,
    ) -> Result<()> {
        let events = session.events.subscribe();
        out_tx
            .send((
                FrameKind::Response,
                ServerMessage::Attached {
                    session_id,
                    backlog: session.snapshot(),
                },
            ))
            .await?;
        let exited = *session.exit_code.lock().expect("exit code mutex poisoned");
        if let Some(exit_code) = exited {
            out_tx
                .send((
                    FrameKind::Notification,
                    ServerMessage::Exited {
                        session_id,
                        exit_code: Some(exit_code),
                    },
                ))
                .await?;
            return Ok(());
        }

        // A client that attaches to a running session starts from a full
        // frame; later patches are relative to it.
        let (frame, state) = session.full_frame()?;
        out_tx
            .send((FrameKind::StreamItem, session_info(session_id, &state)))
            .await?;
        if let SessionEvent::ScreenPatch {
            revision,
            cols,
            rows,
            full,
            dirty_rows,
            cursor,
            viewport,
        } = frame
        {
            out_tx
                .send((
                    FrameKind::StreamItem,
                    ServerMessage::ScreenPatch {
                        session_id,
                        revision,
                        cols,
                        rows,
                        full,
                        dirty_rows,
                        cursor,
                        viewport,
                    },
                ))
                .await?;
        }

        forward_events(session_id, events, out_tx.clone());
        Ok(())
    }

    /// Streams session events to one client until it disconnects.
    fn forward_events(
        session_id: u64,
        mut events: broadcast::Receiver<SessionEvent>,
        forwarding: async_mpsc::Sender<(FrameKind, ServerMessage)>,
    ) {
        tokio::spawn(async move {
            loop {
                let message = match events.recv().await {
                    Ok(SessionEvent::Output(data)) => ServerMessage::Output { session_id, data },
                    Ok(SessionEvent::ScreenPatch {
                        revision,
                        cols,
                        rows,
                        full,
                        dirty_rows,
                        cursor,
                        viewport,
                    }) => ServerMessage::ScreenPatch {
                        session_id,
                        revision,
                        cols,
                        rows,
                        full,
                        dirty_rows,
                        cursor,
                        viewport,
                    },
                    Ok(SessionEvent::Info(state)) => session_info(session_id, &state),
                    Ok(SessionEvent::Clipboard {
                        target,
                        text,
                        program,
                    }) => ServerMessage::ClipboardWrite {
                        session_id,
                        target,
                        text,
                        program,
                    },
                    Ok(SessionEvent::Exited(exit_code)) => {
                        let _ = forwarding
                            .send((
                                FrameKind::Notification,
                                ServerMessage::Exited {
                                    session_id,
                                    exit_code,
                                },
                            ))
                            .await;
                        break;
                    }
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        error!(session_id, skipped, "slow client lost terminal chunks");
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                };
                if forwarding
                    .send((FrameKind::StreamItem, message))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });
    }

    async fn send_error(sender: &async_mpsc::Sender<(FrameKind, ServerMessage)>, message: String) {
        let _ = sender
            .send((FrameKind::Response, ServerMessage::Error { message }))
            .await;
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn backlog_is_bounded_and_keeps_newest_bytes() {
            let backlog = Mutex::new(VecDeque::new());
            append_bounded(&backlog, &vec![1; BACKLOG_LIMIT]);
            append_bounded(&backlog, &[2, 3]);
            let value: Vec<_> = backlog.lock().unwrap().iter().copied().collect();
            assert_eq!(value.len(), BACKLOG_LIMIT);
            assert_eq!(&value[value.len() - 2..], &[2, 3]);
        }
    }
}

#[cfg(any(unix, windows))]
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    daemon::run().await
}
