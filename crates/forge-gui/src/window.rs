//! The Forge window: terminal tabs, splits, palette, notifications and the
//! keyboard/mouse plumbing that turns shell commands into state changes.

use crate::{
    chrome,
    grid_element::{CellMetrics, Palette, TerminalSurface},
    ipc::{IpcCommand, SessionSpec, UiEvent, spawn_ipc_worker},
};
use forge_gui::{
    CellPos, Selection, TerminalGrid,
    config::{Config, ConfigSources, resolve_font_family},
    key_event,
    shell::{
        PaneTree, ShellCommand, ShellContext, ShellKeymap, ShellKeystroke, SplitDirection,
        WindowSession, search_commands,
    },
    theme::{self, ThemeColors},
};
use gpui::{
    App, Bounds, ClipboardItem, Context, EntityInputHandler, FocusHandle, KeyDownEvent, Modifiers,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PathPromptOptions, Pixels,
    PromptLevel, Render, ScrollDelta, ScrollWheelEvent, Timer, UTF16Selection, Window,
    WindowBounds, WindowDecorations, WindowHandle, WindowOptions, prelude::*, px, size,
};
use proto_ipc::{
    KeyAction, KeyMods, MouseAction, MouseButton as TerminalMouseButton, MouseEvent, ScrollRequest,
    ServerMessage, TerminalKey,
};
use std::{
    borrow::Cow,
    ops::Range,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    time::{Duration, Instant},
};
use tokio::sync::{mpsc as async_mpsc, oneshot};

pub const FORGE_APP_ID: &str = "dev.forge.Forge";
/// Interval of the housekeeping tick: config reload, session save and
/// notification expiry. It never redraws unless something changed.
const HOUSEKEEPING: Duration = Duration::from_secs(1);
const NOTIFICATION_TTL: Duration = Duration::from_secs(6);

/// Resolves once the frame that first renders a state change has been handed
/// to the GPU, so benchmarks measure update → present instead of the wait for
/// the next compositor tick.
pub struct FrameProbe {
    pub started: Instant,
    pub presented: oneshot::Sender<f64>,
}

/// Everything a window needs to spawn terminals and open more windows.
#[derive(Clone)]
pub struct WindowFactory {
    pub config_path: Option<PathBuf>,
    pub config: Arc<Config>,
    pub sources: ConfigSources,
    pub socket: PathBuf,
    pub cwd: PathBuf,
    pub metrics: CellMetrics,
    pub window_size: gpui::Size<Pixels>,
    pub render_count: Arc<AtomicU64>,
    /// Benchmarks and headless runs create tabs without a daemon session.
    pub start_ipc: bool,
    /// Restore `session.json` when the window opens.
    pub restore_session: bool,
}

impl WindowFactory {
    pub fn session_path(&self) -> Option<PathBuf> {
        self.config_path
            .as_ref()
            .map(|path| path.with_file_name("session.json"))
    }

    fn spec(&self, cwd: PathBuf, attach: Option<u64>, daemon_instance: Option<u64>) -> SessionSpec {
        let chrome_height = chrome::TOPBAR_HEIGHT + chrome::STATUS_HEIGHT;
        let (cols, rows) = crate::grid_dimensions(self.window_size, self.metrics, chrome_height);
        SessionSpec {
            socket: self.socket.clone(),
            command: self.config.shell(),
            args: self.config.terminal.args.clone(),
            cwd,
            cols,
            rows,
            attach,
            daemon_instance,
        }
    }

    pub fn open(&self, cx: &mut App) -> anyhow::Result<WindowHandle<ForgeWindow>> {
        let factory = self.clone();
        let bounds = Bounds::centered(None, self.window_size, cx);
        let window = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                // Forge owns the chrome whenever the compositor accepts
                // client-side decorations.
                window_decorations: Some(WindowDecorations::Client),
                // Linux desktops associate taskbar icons through this ID and
                // assets/linux/dev.forge.Forge.desktop, not the in-app SVG.
                app_id: Some(FORGE_APP_ID.into()),
                is_resizable: true,
                window_min_size: Some(size(px(480.0), px(320.0))),
                ..Default::default()
            },
            move |window, cx| {
                let view = cx.new(|cx| {
                    cx.observe_window_bounds(window, |view: &mut ForgeWindow, window, _| {
                        view.factory.window_size = window.bounds().size;
                    })
                    .detach();
                    ForgeWindow::new(factory.clone(), cx)
                });
                window.focus(&view.read(cx).focus);
                view
            },
        )?;
        Ok(window)
    }
}

/// A live terminal is owned by exactly one tab, so a background session can
/// never overwrite the grid the user is looking at.
pub struct TerminalTab {
    pub id: u64,
    /// Name shown when the application has not set a title.
    pub default_title: String,
    pub terminal: TerminalSurface,
    pub status: String,
    pub input: async_mpsc::UnboundedSender<IpcCommand>,
    /// Title, cwd and input modes reported by the daemon.
    pub info: SessionInfo,
    /// Daemon session id once attached; persisted for reattach.
    pub session_id: Option<u64>,
    drag_anchor: Option<CellPos>,
    /// Last `(cols, rows)` sent to the daemon; a pane resends only on change.
    viewport: Option<(u16, u16)>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionInfo {
    pub title: String,
    pub pwd: Option<PathBuf>,
    pub mouse_tracking: bool,
    pub alternate_screen: bool,
}

impl TerminalTab {
    pub fn title(&self) -> Cow<'_, str> {
        if let Some(pwd) = &self.info.pwd {
            return pwd
                .file_name()
                .map_or_else(|| pwd.to_string_lossy(), |name| name.to_string_lossy());
        }
        if self.info.title.is_empty() {
            Cow::Borrowed(&self.default_title)
        } else {
            Cow::Borrowed(&self.info.title)
        }
    }

    /// Whether the application, not Forge, should receive mouse events.
    /// Shift bypasses the application, as in every terminal.
    fn app_wants_mouse(&self, modifiers: Modifiers) -> bool {
        self.info.mouse_tracking && !modifiers.shift
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotificationLevel {
    Info,
    Warning,
    Error,
}

pub struct Notification {
    pub text: String,
    pub level: NotificationLevel,
    expires: Instant,
}

#[derive(Default)]
pub struct PaletteState {
    pub open: bool,
    pub query: String,
    pub index: usize,
}

pub struct ForgeWindow {
    pub tabs: Vec<TerminalTab>,
    pub active_tab: usize,
    next_tab_id: u64,
    event_tx: Sender<UiEvent>,
    pub config: Arc<Config>,
    pub theme: ThemeColors,
    pub theme_name: String,
    pub focus: FocusHandle,
    render_count: Arc<AtomicU64>,
    pub frame_probe: Option<FrameProbe>,
    pub factory: WindowFactory,
    pub keymap: ShellKeymap,
    pub palette: PaletteState,
    pub process_explorer: bool,
    pub split: Option<PaneTree>,
    pub notifications: Vec<Notification>,
    /// IME composition in progress, shown in the status line.
    pub marked_text: Option<String>,
    daemon_instance: Option<u64>,
}

impl ForgeWindow {
    fn new(factory: WindowFactory, cx: &mut Context<Self>) -> Self {
        let (event_tx, events) = mpsc::channel();
        Self::spawn_housekeeping(events, cx);
        let (theme, theme_name) = load_theme(&factory);
        let keymap = ShellKeymap::default().with_overrides(&factory.config.keybindings);
        let mut window = Self {
            tabs: Vec::new(),
            active_tab: 0,
            next_tab_id: 1,
            event_tx,
            config: Arc::clone(&factory.config),
            theme,
            theme_name,
            focus: cx.focus_handle(),
            render_count: Arc::clone(&factory.render_count),
            frame_probe: None,
            factory,
            keymap,
            palette: PaletteState::default(),
            process_explorer: false,
            split: None,
            notifications: Vec::new(),
            marked_text: None,
            daemon_instance: None,
        };
        // Tabs must exist before the first draw, which happens inside
        // `open_window`, so the saved layout is applied here.
        match window.load_saved_session() {
            Some(session) => window.apply_saved_session(session, cx),
            None => {
                window.create_terminal_tab(None, cx);
            }
        }
        window
    }

    /// Polls daemon events at frame rate and runs housekeeping once a second.
    /// Nothing here calls `notify` unless state actually changed, which is
    /// what keeps the idle benchmark at zero frames.
    fn spawn_housekeeping(events: Receiver<UiEvent>, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let mut last_tick = Instant::now();
            loop {
                Timer::after(Duration::from_millis(16)).await;
                let mut changed = false;
                while let Ok(event) = events.try_recv() {
                    if this
                        .update(cx, |view, cx| view.handle_event(event, cx))
                        .is_err()
                    {
                        return;
                    }
                    changed = true;
                }
                if last_tick.elapsed() >= HOUSEKEEPING {
                    last_tick = Instant::now();
                    match this.update(cx, ForgeWindow::housekeeping) {
                        Ok(dirty) => changed |= dirty,
                        Err(_) => return,
                    }
                }
                if changed && this.update(cx, |_, cx| cx.notify()).is_err() {
                    return;
                }
            }
        })
        .detach();
    }

    /// Returns whether something visible changed.
    fn housekeeping(&mut self, cx: &mut Context<Self>) -> bool {
        let mut dirty = self.reload_config_if_changed(cx);
        let now = Instant::now();
        let before = self.notifications.len();
        self.notifications.retain(|note| note.expires > now);
        dirty |= self.notifications.len() != before;
        self.save_session();
        dirty
    }

    pub fn notify_user(&mut self, level: NotificationLevel, text: impl Into<String>) {
        let text = text.into();
        eprintln!("forge: {text}");
        // One line in the top bar; the full text went to stderr.
        let text = single_line(&text, 160);
        self.notifications.push(Notification {
            text,
            level,
            expires: Instant::now() + NOTIFICATION_TTL,
        });
    }

    // ----- configuration and theme -------------------------------------

    fn reload_config_if_changed(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(path) = self.factory.config_path.clone() else {
            return false;
        };
        match Config::load_layers(Some(&path), Some(&self.factory.cwd)) {
            Ok((mut config, sources)) => {
                config.font.family =
                    resolve_font_family(&config.font.family, &cx.text_system().all_font_names());
                if config == *self.config && sources == self.factory.sources {
                    return false;
                }
                self.apply_config(config, sources, cx);
                true
            }
            Err(error) => {
                // Keep the running configuration; report once per distinct error.
                let text = format!("Configuración inválida: {error}");
                if !self.notifications.iter().any(|note| note.text == text) {
                    self.notify_user(NotificationLevel::Error, text);
                    return true;
                }
                false
            }
        }
    }

    fn apply_config(&mut self, config: Config, sources: ConfigSources, cx: &mut Context<Self>) {
        let metrics = crate::cell_metrics_for(&config, cx);
        self.keymap = ShellKeymap::default().with_overrides(&config.keybindings);
        self.factory.metrics = metrics;
        self.factory.sources = sources;
        self.factory.config = Arc::new(config);
        self.config = Arc::clone(&self.factory.config);
        let (theme, name) = load_theme(&self.factory);
        self.set_theme(theme, name);
        for tab in &mut self.tabs {
            tab.terminal.metrics = metrics;
            tab.viewport = None;
        }
        self.notify_user(NotificationLevel::Info, "Configuración recargada");
        cx.notify();
    }

    fn set_theme(&mut self, theme: ThemeColors, name: String) {
        self.theme = theme;
        self.theme_name = name;
        let palette = Palette::from(&theme);
        for tab in &mut self.tabs {
            tab.terminal.palette = palette;
        }
    }

    fn cycle_theme(&mut self, cx: &mut Context<Self>) {
        let dir = self.themes_dir();
        let names = theme::available_themes(dir.as_deref());
        let position = names
            .iter()
            .position(|name| name.eq_ignore_ascii_case(&self.theme_name))
            .unwrap_or(0);
        let next = names[(position + 1) % names.len()].clone();
        match theme::load_theme(&next, dir.as_deref()) {
            Ok(colors) => {
                let colors = colors.with_overrides(&self.config.colors);
                self.set_theme(colors, next.clone());
                self.notify_user(NotificationLevel::Info, format!("Tema: {next}"));
            }
            Err(error) => self.notify_user(NotificationLevel::Error, error),
        }
        cx.notify();
    }

    fn themes_dir(&self) -> Option<PathBuf> {
        self.factory
            .config_path
            .as_deref()
            .and_then(theme::themes_dir)
    }

    // ----- session persistence ------------------------------------------

    fn session(&self) -> WindowSession {
        WindowSession {
            version: WindowSession::VERSION,
            count: self.tabs.len(),
            active: self.active_tab,
            split: self.split.clone(),
            cwd: self.factory.cwd.clone(),
            width: f32::from(self.factory.window_size.width),
            height: f32::from(self.factory.window_size.height),
            theme: Some(self.theme_name.clone()),
            sessions: self.tabs.iter().map(|tab| tab.session_id).collect(),
            daemon_instance: self.daemon_instance,
        }
    }

    fn save_session(&mut self) {
        if !self.factory.restore_session {
            return;
        }
        let Some(path) = self.factory.session_path() else {
            return;
        };
        if let Err(error) = self.session().save(&path) {
            self.notify_user(
                NotificationLevel::Warning,
                format!("No se pudo guardar la sesión: {error}"),
            );
        }
    }

    fn load_saved_session(&mut self) -> Option<WindowSession> {
        if !self.factory.restore_session {
            return None;
        }
        let path = self.factory.session_path()?;
        match WindowSession::load(&path) {
            Ok(session) => session,
            Err(error) => {
                self.notify_user(
                    NotificationLevel::Warning,
                    format!("Sesión guardada inválida, se ignora: {error}"),
                );
                None
            }
        }
    }

    /// Recreates the saved tabs, reattaching to the daemon sessions that
    /// survived the previous Forge process.
    fn apply_saved_session(&mut self, session: WindowSession, cx: &mut Context<Self>) {
        if session.cwd.is_dir() {
            self.factory.cwd = session.cwd.clone();
        }
        for index in 0..session.count {
            self.open_tab(
                None,
                session.daemon_session(index),
                session.daemon_instance,
                cx,
            );
        }
        self.active_tab = session.active.min(self.tabs.len().saturating_sub(1));
        self.split = session.split;
        if let Some(name) = session.theme
            && !name.eq_ignore_ascii_case(&self.theme_name)
            && let Ok(colors) = theme::load_theme(&name, self.themes_dir().as_deref())
        {
            self.set_theme(colors.with_overrides(&self.config.colors), name);
        }
        cx.notify();
    }

    // ----- tabs and panes -----------------------------------------------

    pub fn active_tab(&self) -> &TerminalTab {
        &self.tabs[self.active_tab]
    }

    pub fn active_tab_mut(&mut self) -> &mut TerminalTab {
        &mut self.tabs[self.active_tab]
    }

    fn tab_mut(&mut self, id: u64) -> Option<&mut TerminalTab> {
        self.tabs.iter_mut().find(|tab| tab.id == id)
    }

    /// Surface of pane `index`, called from the grid element's paint with the
    /// area it fills; the PTY is resized when that area changes.
    pub fn pane_surface(&mut self, index: usize, bounds: Bounds<Pixels>) -> &mut TerminalSurface {
        let (cols, rows) = crate::grid_dimensions(bounds.size, self.factory.metrics, 0.0);
        let tab = &mut self.tabs[index];
        if tab.viewport != Some((cols, rows)) {
            tab.viewport = Some((cols, rows));
            let _ = tab.input.send(IpcCommand::Resize { cols, rows });
        }
        &mut tab.terminal
    }

    /// Creates a tab (and its daemon session) and makes it active. Without an
    /// explicit directory the new shell starts where the active one is (OSC 7).
    pub fn create_terminal_tab(&mut self, cwd: Option<PathBuf>, cx: &mut Context<Self>) -> usize {
        self.open_tab(cwd, None, None, cx)
    }

    /// Like [`Self::create_terminal_tab`], reattaching to a daemon session
    /// from a previous run when it is still alive.
    fn open_tab(
        &mut self,
        cwd: Option<PathBuf>,
        attach: Option<u64>,
        daemon_instance: Option<u64>,
        cx: &mut Context<Self>,
    ) -> usize {
        let cwd = cwd.or_else(|| {
            self.tabs
                .get(self.active_tab)
                .and_then(|tab| tab.info.pwd.clone())
                .filter(|path| path.is_dir())
        });
        let id = self.next_tab_id;
        self.next_tab_id += 1;
        let (input, input_rx) = async_mpsc::unbounded_channel();
        let title = cwd.as_ref().and_then(|path| path.file_name()).map_or_else(
            || format!("Terminal {id}"),
            |name| name.to_string_lossy().into_owned(),
        );
        let chrome_height = chrome::TOPBAR_HEIGHT + chrome::STATUS_HEIGHT;
        let (initial_cols, initial_rows) = crate::grid_dimensions(
            self.factory.window_size,
            self.factory.metrics,
            chrome_height,
        );
        self.tabs.push(TerminalTab {
            id,
            default_title: title,
            terminal: TerminalSurface::new(
                TerminalGrid::new(initial_cols, initial_rows),
                self.factory.metrics,
                Palette::from(&self.theme),
            ),
            status: if self.factory.start_ipc {
                "Conectando a forge-termd…".into()
            } else {
                "Panel vacío".into()
            },
            input,
            info: SessionInfo::default(),
            session_id: None,
            drag_anchor: None,
            viewport: None,
        });
        self.active_tab = self.tabs.len() - 1;
        if self.factory.start_ipc {
            let cwd = cwd.unwrap_or_else(|| self.factory.cwd.clone());
            spawn_ipc_worker(
                self.factory.spec(cwd, attach, daemon_instance),
                id,
                self.event_tx.clone(),
                input_rx,
            );
        }
        cx.notify();
        self.active_tab
    }

    pub fn activate_tab(&mut self, index: usize, cx: &mut Context<Self>) {
        if index < self.tabs.len() && index != self.active_tab {
            self.active_tab = index;
            cx.notify();
        }
    }

    pub fn close_tab(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.tabs.len() <= 1 || index >= self.tabs.len() {
            return;
        }
        // Closing a tab ends its shell; sessions only survive when the
        // window itself goes away.
        let closed = self.tabs.remove(index);
        let _ = closed.input.send(IpcCommand::Shutdown);
        self.active_tab = if self.active_tab > index {
            self.active_tab - 1
        } else {
            self.active_tab.min(self.tabs.len() - 1)
        };
        self.split = self.split.take().and_then(|tree| tree.remove(index));
        cx.notify();
    }

    /// The split tree to render: the stored one when it still contains the
    /// active tab, otherwise the active tab alone.
    pub fn visible_tree(&self) -> PaneTree {
        self.split
            .clone()
            .filter(|tree| tree.contains(self.active_tab))
            .unwrap_or(PaneTree::leaf(self.active_tab))
    }

    fn split_active(&mut self, direction: SplitDirection, cx: &mut Context<Self>) {
        let first = self.active_tab;
        let second = self.create_terminal_tab(None, cx);
        self.split
            .get_or_insert(PaneTree::leaf(first))
            .split(first, second, direction);
        self.active_tab = second;
        cx.notify();
    }

    // ----- events from the daemon ---------------------------------------

    fn handle_event(&mut self, event: UiEvent, cx: &mut Context<Self>) {
        match event {
            UiEvent::Message {
                tab_id,
                message: ServerMessage::Exited { exit_code, .. },
            } => {
                let index = self.tabs.iter().position(|tab| tab.id == tab_id);
                match index {
                    // A shell that exits closes its tab; the last one closes
                    // the window like any terminal emulator.
                    Some(index) if self.tabs.len() > 1 => self.close_tab(index, cx),
                    Some(_) => cx.quit(),
                    None => {}
                }
                if let Some(code) = exit_code.filter(|code| *code != 0) {
                    self.notify_user(
                        NotificationLevel::Warning,
                        format!("La shell terminó con código {code}"),
                    );
                }
            }
            UiEvent::Message {
                tab_id,
                message: ServerMessage::Error { message },
            } => {
                if let Some(tab) = self.tab_mut(tab_id) {
                    tab.status = format!("Error del daemon: {message}");
                }
            }
            UiEvent::Message {
                tab_id,
                message:
                    ServerMessage::SessionInfo {
                        title,
                        pwd,
                        mouse_tracking,
                        alternate_screen,
                        ..
                    },
            } => {
                if let Some(tab) = self.tab_mut(tab_id) {
                    tab.info = SessionInfo {
                        title,
                        pwd: pwd.map(PathBuf::from),
                        mouse_tracking,
                        alternate_screen,
                    };
                }
            }
            UiEvent::Message { tab_id, message } => {
                let Some(tab) = self.tab_mut(tab_id) else {
                    return;
                };
                if let ServerMessage::ScreenPatch {
                    revision,
                    full,
                    dirty_rows,
                    viewport,
                    ..
                } = &message
                {
                    tracing::debug!(
                        tab_id,
                        revision,
                        full,
                        rows = dirty_rows.len(),
                        ?viewport,
                        "screen patch"
                    );
                }
                let before = tab.terminal.grid.dimensions();
                match tab.terminal.grid.apply_server_message(message) {
                    Ok(true) => {
                        if tab.terminal.grid.dimensions() != before {
                            tab.terminal.selection = None;
                        }
                        tab.status =
                            format!("Sesión activa · revisión {}", tab.terminal.grid.revision());
                    }
                    Ok(false) => {}
                    Err(error) => tab.status = format!("Patch inválido: {error}"),
                }
            }
            UiEvent::Status { tab_id, status } => {
                if let Some(tab) = self.tab_mut(tab_id) {
                    tab.status = status;
                }
            }
            UiEvent::Attached {
                tab_id,
                session_id,
                daemon_instance,
            } => {
                self.daemon_instance = Some(daemon_instance);
                if let Some(tab) = self.tab_mut(tab_id) {
                    tab.session_id = Some(session_id);
                }
                self.save_session();
            }
        }
    }

    // ----- keyboard -----------------------------------------------------

    pub fn on_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The terminal consumes every key. Without this, GPUI's Linux backends
        // would also hand `key_char` to the IME input handler and each typed
        // character would reach the shell twice.
        cx.stop_propagation();
        let keystroke = &event.keystroke;
        let modifiers = key_mods(keystroke.modifiers);
        if self.palette.open {
            self.palette_key(
                keystroke.key.as_str(),
                keystroke.key_char.as_deref(),
                modifiers,
                window,
                cx,
            );
            return;
        }
        let shell_key = ShellKeystroke::new(
            &keystroke.key,
            modifiers.control,
            modifiers.alt,
            modifiers.shift,
        );
        if let Some(command) = self.keymap.resolve(&shell_key, ShellContext::Terminal) {
            self.run_shell_command(command, window, cx);
            return;
        }
        let copy_paste = modifiers.control && modifiers.shift && !modifiers.alt;
        let scrollback = modifiers.shift && !modifiers.control && !modifiers.alt;
        match keystroke.key.as_str() {
            "c" if copy_paste => {
                self.copy_selection(cx);
                return;
            }
            "v" if copy_paste => {
                self.paste(cx.read_from_clipboard());
                return;
            }
            "insert" if scrollback => {
                self.paste(read_primary(cx));
                return;
            }
            "pageup" if scrollback => {
                self.scroll_active(ScrollRequest::Delta(-self.page_rows()));
                return;
            }
            "pagedown" if scrollback => {
                self.scroll_active(ScrollRequest::Delta(self.page_rows()));
                return;
            }
            "home" if scrollback => {
                self.scroll_active(ScrollRequest::Top);
                return;
            }
            "end" if scrollback => {
                self.scroll_active(ScrollRequest::Bottom);
                return;
            }
            _ => {}
        }
        let event = key_event(
            &keystroke.key,
            keystroke.key_char.as_deref(),
            modifiers,
            if event.is_held {
                KeyAction::Repeat
            } else {
                KeyAction::Press
            },
        );
        // Bare modifiers reach the daemon too: the Kitty protocol reports them.
        if event.key != TerminalKey::Unidentified || event.text.is_some() {
            let tab = self.active_tab_mut();
            tab.terminal.selection = None;
            let _ = tab.input.send(IpcCommand::Key(event));
            cx.notify();
        }
    }

    /// Rows scrolled by Shift+PageUp/PageDown: one screen minus a line.
    fn page_rows(&self) -> i64 {
        let (_, rows) = self.active_tab().terminal.grid.dimensions();
        i64::from(rows.saturating_sub(1).max(1))
    }

    fn scroll_active(&mut self, scroll: ScrollRequest) {
        let _ = self.active_tab().input.send(IpcCommand::Scroll(scroll));
    }

    /// Mouse wheel: the application gets it while it tracks the mouse;
    /// otherwise it moves the viewport over the scrollback.
    pub fn on_scroll_wheel(&mut self, event: &ScrollWheelEvent, _cx: &mut Context<Self>) {
        let Some(index) = self
            .tabs
            .iter()
            .position(|tab| tab.terminal.contains(event.position))
        else {
            return;
        };
        let tab = &self.tabs[index];
        let lines = match event.delta {
            ScrollDelta::Lines(delta) => delta.y,
            ScrollDelta::Pixels(delta) => f32::from(delta.y) / tab.terminal.metrics.height,
        };
        #[allow(clippy::cast_possible_truncation)]
        let rows = lines.round() as i64;
        if rows == 0 {
            return;
        }
        if tab.app_wants_mouse(event.modifiers) {
            let Some(cell) = tab.terminal.cell_at(event.position) else {
                return;
            };
            let button = if rows > 0 {
                TerminalMouseButton::WheelUp
            } else {
                TerminalMouseButton::WheelDown
            };
            for _ in 0..rows.unsigned_abs().min(8) {
                let _ = tab.input.send(IpcCommand::Mouse(MouseEvent {
                    action: MouseAction::Press,
                    button: Some(button),
                    mods: key_mods(event.modifiers),
                    col: cell.x,
                    row: cell.y,
                }));
            }
        } else {
            // Wheel up is a positive delta in GPUI and scrolls the viewport
            // towards older rows, which is negative for the daemon.
            let _ = tab
                .input
                .send(IpcCommand::Scroll(ScrollRequest::Delta(-rows)));
        }
    }

    fn palette_key(
        &mut self,
        key: &str,
        key_char: Option<&str>,
        modifiers: KeyMods,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match key {
            "escape" => self.palette.open = false,
            "enter" => {
                let command = search_commands(&self.palette.query)
                    .get(self.palette.index)
                    .map(|item| item.command);
                self.palette.open = false;
                if let Some(command) = command {
                    self.run_shell_command(command, window, cx);
                }
            }
            "backspace" => {
                self.palette.query.pop();
                self.palette.index = 0;
            }
            "up" => self.palette.index = self.palette.index.saturating_sub(1),
            "down" => {
                let count = search_commands(&self.palette.query).len();
                self.palette.index = (self.palette.index + 1).min(count.saturating_sub(1));
            }
            _ if !modifiers.control && !modifiers.alt => {
                if let Some(text) = key_char {
                    self.palette.query.push_str(text);
                    self.palette.index = 0;
                }
            }
            _ => {}
        }
        cx.notify();
    }

    pub fn run_shell_command(
        &mut self,
        command: ShellCommand,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        tracing::debug!(
            command = command.id(),
            tab = self.active_tab().id,
            "shell command"
        );
        match command {
            ShellCommand::NewTerminalTab => {
                self.create_terminal_tab(None, cx);
            }
            ShellCommand::NewTerminalTabInDirectory => Self::prompt_for_directory(cx),
            ShellCommand::CloseWindow if self.tabs.len() > 1 => {
                self.close_tab(self.active_tab, cx);
            }
            ShellCommand::CloseWindow => Self::confirm_close_window(window, cx),
            ShellCommand::ToggleMaximize => window.zoom_window(),
            ShellCommand::ShowCommandPalette => {
                self.palette = PaletteState {
                    open: true,
                    ..PaletteState::default()
                };
                cx.notify();
            }
            ShellCommand::SplitHorizontal => self.split_active(SplitDirection::Horizontal, cx),
            ShellCommand::SplitVertical => self.split_active(SplitDirection::Vertical, cx),
            ShellCommand::FocusNextPane => {
                let next = self.visible_tree().next_leaf(self.active_tab);
                let next = if next == self.active_tab && self.tabs.len() > 1 {
                    (self.active_tab + 1) % self.tabs.len()
                } else {
                    next
                };
                self.activate_tab(next, cx);
            }
            ShellCommand::ShowProcessExplorer => {
                self.process_explorer = !self.process_explorer;
                cx.notify();
            }
            ShellCommand::CycleTheme => self.cycle_theme(cx),
            ShellCommand::ReloadConfig => {
                if !self.reload_config_if_changed(cx) {
                    self.notify_user(NotificationLevel::Info, "Configuración sin cambios");
                    cx.notify();
                }
            }
        }
    }

    /// Native directory picker; the chosen directory becomes a new tab's cwd.
    fn prompt_for_directory(cx: &mut Context<Self>) {
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Abrir terminal en…".into()),
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = receiver.await
                && let Some(path) = paths.into_iter().next()
            {
                let _ = this.update(cx, |view, cx| {
                    view.create_terminal_tab(Some(path), cx);
                });
            }
        })
        .detach();
    }

    /// Native confirmation before closing the last terminal of the window.
    fn confirm_close_window(window: &mut Window, cx: &mut Context<Self>) {
        let answer = window.prompt(
            PromptLevel::Warning,
            "¿Cerrar la ventana de Forge?",
            Some("La sesión de terminal sigue viva en forge-termd."),
            &["Cerrar", "Cancelar"],
            cx,
        );
        cx.spawn(async move |this, cx| {
            if answer.await == Ok(0) {
                let _ = this.update(cx, |_, cx| cx.quit());
            }
        })
        .detach();
    }

    // ----- clipboard and mouse selection --------------------------------

    fn selected_text(&self) -> Option<String> {
        let terminal = &self.active_tab().terminal;
        let text = terminal.grid.selected_text(terminal.selection?);
        (!text.is_empty()).then_some(text)
    }

    fn copy_selection(&self, cx: &mut Context<Self>) {
        if let Some(text) = self.selected_text() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    /// The daemon applies bracketed paste when the application asked for it.
    fn paste(&self, item: Option<ClipboardItem>) {
        if let Some(text) = item.and_then(|item| item.text()) {
            let _ = self.active_tab().input.send(IpcCommand::Paste(text));
        }
    }

    pub fn on_mouse_down(&mut self, event: &MouseDownEvent, cx: &mut Context<Self>) {
        let Some(index) = self
            .tabs
            .iter()
            .position(|tab| tab.terminal.contains(event.position))
        else {
            return;
        };
        self.activate_tab(index, cx);
        let Some(cell) = self.active_tab().terminal.cell_at(event.position) else {
            return;
        };
        let tab = self.active_tab_mut();
        if tab.app_wants_mouse(event.modifiers) {
            tab.drag_anchor = None;
            let _ = tab.input.send(IpcCommand::Mouse(MouseEvent {
                action: MouseAction::Press,
                button: Some(gpui_button(event.button)),
                mods: key_mods(event.modifiers),
                col: cell.x,
                row: cell.y,
            }));
            return;
        }
        if event.button != MouseButton::Left {
            return;
        }
        tab.terminal.selection = match event.click_count {
            2 => Some(tab.terminal.grid.word_at(cell)),
            n if n >= 3 => Some(tab.terminal.grid.line_at(cell.y)),
            _ => None,
        };
        tab.drag_anchor = Some(cell);
        cx.notify();
    }

    pub fn on_mouse_move(&mut self, event: &MouseMoveEvent, cx: &mut Context<Self>) {
        let tab = self.active_tab();
        if tab.app_wants_mouse(event.modifiers) && tab.terminal.contains(event.position) {
            if let Some(cell) = tab.terminal.cell_at(event.position) {
                let _ = tab.input.send(IpcCommand::Mouse(MouseEvent {
                    action: MouseAction::Motion,
                    button: event.pressed_button.map(gpui_button),
                    mods: key_mods(event.modifiers),
                    col: cell.x,
                    row: cell.y,
                }));
            }
            return;
        }
        let Some(anchor) = self.active_tab().drag_anchor else {
            return;
        };
        if event.pressed_button != Some(MouseButton::Left) {
            self.active_tab_mut().drag_anchor = None;
            return;
        }
        let Some(head) = self.active_tab().terminal.cell_at(event.position) else {
            return;
        };
        let selection = Some(Selection { anchor, head });
        if self.active_tab().terminal.selection != selection {
            self.active_tab_mut().terminal.selection = selection;
            cx.notify();
        }
    }

    pub fn on_mouse_up(&mut self, event: &MouseUpEvent, cx: &mut Context<Self>) {
        let tab = self.active_tab();
        if tab.app_wants_mouse(event.modifiers) && tab.drag_anchor.is_none() {
            if let Some(cell) = tab.terminal.cell_at(event.position) {
                let _ = tab.input.send(IpcCommand::Mouse(MouseEvent {
                    action: MouseAction::Release,
                    button: Some(gpui_button(event.button)),
                    mods: key_mods(event.modifiers),
                    col: cell.x,
                    row: cell.y,
                }));
            }
            return;
        }
        if self.active_tab_mut().drag_anchor.take().is_none() {
            return;
        }
        // Linux convention: a finished selection is available on middle click.
        if let Some(text) = self.selected_text() {
            write_primary(cx, ClipboardItem::new_string(text));
        }
    }

    pub fn render_count(&self) -> u64 {
        self.render_count.load(Ordering::Relaxed)
    }

    // ----- benchmarks ----------------------------------------------------

    /// Opens `count` empty panes in a balanced split tree: leaf `k` is split
    /// by the panes `2k+1` and `2k+2`, like a binary heap, so the depth stays
    /// logarithmic. (A chain of nested splits makes flex layout superlinear.)
    pub fn open_benchmark_panes(&mut self, count: usize, cx: &mut Context<Self>) {
        while self.tabs.len() < count {
            let new = self.create_terminal_tab(None, cx);
            let target = (new - 1) / 2;
            let direction = if new.is_multiple_of(2) {
                SplitDirection::Vertical
            } else {
                SplitDirection::Horizontal
            };
            self.split
                .get_or_insert(PaneTree::leaf(0))
                .split(target, new, direction);
        }
        self.active_tab = 0;
        cx.notify();
    }
}

impl Render for ForgeWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.render_count.fetch_add(1, Ordering::Relaxed);
        if let Some(probe) = self.frame_probe.take() {
            // Deferred callbacks run when the outermost update finishes, which
            // during a draw is right after GPUI presented this frame.
            cx.defer(move |_| {
                let _ = probe
                    .presented
                    .send(probe.started.elapsed().as_secs_f64() * 1_000.0);
            });
        }
        chrome::render_window(self, window, cx)
    }
}

/// Multi-character IME commits (CJK, emoji pickers) and dead-key composition
/// on macOS arrive here instead of as key presses.
impl EntityInputHandler for ForgeWindow {
    fn text_for_range(
        &mut self,
        _range: Range<usize>,
        _adjusted_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        None
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: 0..0,
            reversed: false,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        self.marked_text
            .as_ref()
            .map(|text| 0..text.encode_utf16().count())
    }

    fn unmark_text(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.marked_text.take().is_some() {
            cx.notify();
        }
    }

    // Wayland sends an IME `done` without preedit on every focus change, so
    // these only redraw when something visible actually changed: otherwise
    // an idle window would render for nothing.
    fn replace_text_in_range(
        &mut self,
        _range: Option<Range<usize>>,
        text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let had_preedit = self.marked_text.take().is_some();
        if !text.is_empty() {
            let tab = self.active_tab_mut();
            tab.terminal.selection = None;
            let _ = tab.input.send(IpcCommand::Key(key_event(
                "",
                Some(text),
                KeyMods::default(),
                KeyAction::Press,
            )));
            cx.notify();
        } else if had_preedit {
            cx.notify();
        }
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _range: Option<Range<usize>>,
        new_text: &str,
        _new_selected_range: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let marked = (!new_text.is_empty()).then(|| new_text.to_string());
        if self.marked_text != marked {
            self.marked_text = marked;
            cx.notify();
        }
    }

    fn bounds_for_range(
        &mut self,
        _range_utf16: Range<usize>,
        _element_bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        self.active_tab().terminal.cursor_bounds()
    }

    fn character_index_for_point(
        &mut self,
        _point: gpui::Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }
}

/// The X11/Wayland primary selection; other platforms have no equivalent.
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
fn read_primary(cx: &App) -> Option<ClipboardItem> {
    cx.read_from_primary()
}

#[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
fn read_primary(cx: &App) -> Option<ClipboardItem> {
    cx.read_from_clipboard()
}

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
fn write_primary(cx: &App, item: ClipboardItem) {
    cx.write_to_primary(item);
}

#[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
fn write_primary(_cx: &App, _item: ClipboardItem) {}

/// Theme from the factory's config, with config colour overrides applied.
/// An unknown theme falls back to `forge-dark` and says so on stderr.
fn load_theme(factory: &WindowFactory) -> (ThemeColors, String) {
    let dir = factory.config_path.as_deref().and_then(theme::themes_dir);
    let requested = &factory.config.ui.theme;
    match theme::load_theme(requested, dir.as_deref()) {
        Ok(colors) => (
            colors.with_overrides(&factory.config.colors),
            requested.clone(),
        ),
        Err(error) => {
            eprintln!("forge: {error}; usando {}", theme::FORGE_DARK);
            (
                ThemeColors::forge_dark().with_overrides(&factory.config.colors),
                theme::FORGE_DARK.into(),
            )
        }
    }
}

/// Collapses whitespace and truncates for display in a single-line bar.
fn single_line(text: &str, max_chars: usize) -> String {
    let mut line: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() > max_chars {
        line = line.chars().take(max_chars - 1).collect::<String>() + "…";
    }
    line
}

fn key_mods(modifiers: Modifiers) -> KeyMods {
    KeyMods {
        shift: modifiers.shift,
        control: modifiers.control,
        alt: modifiers.alt,
        super_key: modifiers.platform,
    }
}

fn gpui_button(button: MouseButton) -> TerminalMouseButton {
    match button {
        MouseButton::Left => TerminalMouseButton::Left,
        MouseButton::Right => TerminalMouseButton::Right,
        MouseButton::Middle => TerminalMouseButton::Middle,
        MouseButton::Navigate(gpui::NavigationDirection::Back) => TerminalMouseButton::Back,
        MouseButton::Navigate(gpui::NavigationDirection::Forward) => TerminalMouseButton::Forward,
    }
}

/// Where the configured `cwd` for new tabs comes from at startup.
pub fn initial_cwd(session_cwd: Option<&Path>) -> PathBuf {
    session_cwd
        .filter(|path| path.is_dir())
        .map(Path::to_path_buf)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notifications_collapse_to_one_line() {
        assert_eq!(single_line("a\n  b\tc", 10), "a b c");
        assert_eq!(single_line("abcdefghij", 5), "abcd…");
        assert_eq!(single_line("", 5), "");
    }
}
