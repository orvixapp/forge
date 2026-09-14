//! The Forge window: terminal tabs, splits, palette, notifications and the
//! keyboard/mouse plumbing that turns shell commands into state changes.

use crate::{
    agent::{AgentTab, PromptContext, spawn_agent_worker},
    chrome,
    editor::EditorTab,
    grid_element::{CellMetrics, Palette, SearchHighlights, TerminalSurface},
    ipc::{IpcCommand, SessionSpec, UiEvent, spawn_ipc_worker},
    project::{EditorFind, Finder, FinderMode, ProjectState},
    search::{SearchAction, SearchDirection, SearchState, reveal_row},
};
use forge_gui::{
    CellPos, Selection, TerminalGrid,
    config::{ClipboardPolicy, Config, ConfigSources, TerminalProfile, resolve_font_family},
    key_event,
    links::{self, LinkTarget, PasteRisk},
    shell::{
        PaneTree, ShellCommand, ShellContext, ShellKeymap, ShellKeystroke, SplitDirection,
        WindowSession, integration_launch, search_commands,
    },
    theme::{self, ThemeColors},
};
use gpui::point;
use gpui::{
    App, Bounds, ClipboardItem, Context, EntityInputHandler, FocusHandle, KeyDownEvent, Modifiers,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PathPromptOptions, Pixels, Point,
    PromptLevel, Render, ScrollDelta, ScrollWheelEvent, Timer, UTF16Selection, Window,
    WindowBounds, WindowDecorations, WindowHandle, WindowOptions, prelude::*, px, size,
};
use proto_ipc::{
    ClipboardTarget, KeyAction, KeyMods, MouseAction, MouseButton as TerminalMouseButton,
    MouseEvent, ProcessSignal, PromptDirection, ScrollRequest, ServerMessage, TerminalKey,
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

/// Where the shell integration scripts live: `$FORGE_SHELL_INTEGRATION`,
/// else the repository's `assets/` during development.
pub fn shell_integration_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("FORGE_SHELL_INTEGRATION") {
        return Some(PathBuf::from(dir)).filter(|dir| dir.is_dir());
    }
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()?
        .parent()?
        .join("assets")
        .join("shell-integration");
    repo.is_dir().then_some(repo)
}

impl WindowFactory {
    pub fn session_path(&self) -> Option<PathBuf> {
        self.config_path
            .as_ref()
            .map(|path| path.with_file_name("session.json"))
    }

    fn spec(
        &self,
        cwd: PathBuf,
        attach: Option<u64>,
        daemon_instance: Option<u64>,
        profile: Option<&TerminalProfile>,
    ) -> SessionSpec {
        let chrome_height = chrome::TOPBAR_HEIGHT + chrome::STATUS_HEIGHT;
        let (cols, rows) = crate::grid_dimensions(self.window_size, self.metrics, chrome_height);
        let command = profile
            .and_then(|profile| profile.shell.clone())
            .unwrap_or_else(|| self.config.shell());
        let args = profile.map_or_else(
            || self.config.terminal.args.clone(),
            |profile| profile.args.clone(),
        );
        let (args, mut env) = integration_launch(
            &command,
            args,
            shell_integration_dir().as_deref(),
            self.config.terminal.shell_integration,
        );
        if let Some(profile) = profile {
            env.extend(
                profile
                    .env
                    .iter()
                    .map(|(key, value)| (key.clone(), value.clone())),
            );
        }
        SessionSpec {
            socket: self.socket.clone(),
            command,
            args,
            cwd,
            cols,
            rows,
            env,
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

/// One tab of the window: a terminal or an editor, both addressable by the
/// pane tree through their index and by events through their id.
pub struct Tab {
    pub id: u64,
    /// Name given by the user (`terminal.renameTab`); wins over everything.
    pub custom_title: Option<String>,
    pub content: TabContent,
}

pub enum TabContent {
    Terminal(Box<TerminalTab>),
    Editor(Box<EditorTab>),
    Agent(Box<AgentTab>),
}

impl Tab {
    pub fn title(&self) -> Cow<'_, str> {
        if let Some(custom) = &self.custom_title {
            return Cow::Borrowed(custom);
        }
        match &self.content {
            TabContent::Terminal(terminal) => terminal.title(),
            TabContent::Editor(editor) => editor.title(),
            TabContent::Agent(agent) => Cow::Owned(format!("Agente · {}", agent.agent_name)),
        }
    }

    pub fn status(&self) -> String {
        match &self.content {
            TabContent::Terminal(terminal) => terminal.status.clone(),
            TabContent::Editor(editor) => editor.status(),
            TabContent::Agent(agent) => agent.status.clone(),
        }
    }

    pub fn terminal(&self) -> Option<&TerminalTab> {
        match &self.content {
            TabContent::Terminal(terminal) => Some(terminal),
            TabContent::Editor(_) | TabContent::Agent(_) => None,
        }
    }

    pub fn terminal_mut(&mut self) -> Option<&mut TerminalTab> {
        match &mut self.content {
            TabContent::Terminal(terminal) => Some(terminal),
            TabContent::Editor(_) | TabContent::Agent(_) => None,
        }
    }

    pub fn editor(&self) -> Option<&EditorTab> {
        match &self.content {
            TabContent::Editor(editor) => Some(editor),
            TabContent::Terminal(_) | TabContent::Agent(_) => None,
        }
    }

    pub fn editor_mut(&mut self) -> Option<&mut EditorTab> {
        match &mut self.content {
            TabContent::Editor(editor) => Some(editor),
            TabContent::Terminal(_) | TabContent::Agent(_) => None,
        }
    }

    pub fn agent(&self) -> Option<&AgentTab> {
        match &self.content {
            TabContent::Agent(agent) => Some(agent),
            TabContent::Terminal(_) | TabContent::Editor(_) => None,
        }
    }

    pub fn agent_mut(&mut self) -> Option<&mut AgentTab> {
        match &mut self.content {
            TabContent::Agent(agent) => Some(agent),
            TabContent::Terminal(_) | TabContent::Editor(_) => None,
        }
    }

    /// Whether a window position lies on the tab's content.
    fn contains(&self, position: Point<Pixels>) -> bool {
        match &self.content {
            TabContent::Terminal(terminal) => terminal.terminal.contains(position),
            TabContent::Editor(editor) => editor.contains(position),
            TabContent::Agent(_) => false,
        }
    }
}

/// A live terminal is owned by exactly one tab, so a background session can
/// never overwrite the grid the user is looking at.
pub struct TerminalTab {
    /// Name shown when the application has not set a title.
    pub default_title: String,
    pub terminal: TerminalSurface,
    pub status: String,
    pub input: async_mpsc::UnboundedSender<IpcCommand>,
    /// Title, cwd and input modes reported by the daemon.
    pub info: SessionInfo,
    /// Daemon session id once attached; persisted for reattach.
    pub session_id: Option<u64>,
    /// The user allowed OSC 52 writes from this tab for its lifetime.
    clipboard_allowed: bool,
    drag_anchor: Option<CellPos>,
    /// Last `(cols, rows)` sent to the daemon; a pane resends only on change.
    viewport: Option<(u16, u16)>,
    agent_owner: Option<u64>,
    agent_output: String,
    agent_output_limit: usize,
    agent_output_truncated: bool,
    agent_exited: bool,
    agent_exit_code: Option<u32>,
    agent_waiters: Vec<(
        serde_json::Value,
        oneshot::Sender<proto_acp::JsonRpcMessage>,
    )>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionInfo {
    pub title: String,
    pub pwd: Option<PathBuf>,
    pub mouse_tracking: bool,
    pub alternate_screen: bool,
    pub bracketed_paste: bool,
}

/// A question shown inside the window; Enter/`y` accepts, Esc/`n` declines
/// and, for clipboard writes, `a` accepts for the rest of the tab.
pub struct Confirmation {
    pub title: String,
    pub body: String,
    pub kind: ConfirmationKind,
}

pub enum ConfirmationKind {
    /// Paste `text` into the active tab as-is.
    Paste(String),
    /// Apply an OSC 52 write requested by tab `tab_id`.
    Clipboard {
        tab_id: u64,
        target: ClipboardTarget,
        text: String,
    },
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

/// Right-click menu: commands listed at the pointer, run on click or
/// Enter; any other click, or Esc, dismisses it.
pub struct ContextMenu {
    pub position: Point<Pixels>,
    pub items: Vec<ShellCommand>,
    pub index: usize,
}

/// Single-line text prompt inside the window (tab rename).
pub struct TextPrompt {
    pub title: String,
    pub value: String,
}

/// List picker inside the window (profiles); `index` selects an item.
pub struct Picker {
    pub title: String,
    pub items: Vec<String>,
    pub index: usize,
}

pub struct ForgeWindow {
    pub tabs: Vec<Tab>,
    pub active_tab: usize,
    next_tab_id: u64,
    pub(crate) event_tx: Sender<UiEvent>,
    pub config: Arc<Config>,
    pub theme: ThemeColors,
    pub theme_name: String,
    pub focus: FocusHandle,
    render_count: Arc<AtomicU64>,
    pub frame_probe: Option<FrameProbe>,
    pub factory: WindowFactory,
    pub keymap: ShellKeymap,
    pub palette: PaletteState,
    pub search: SearchState,
    pub finder: Option<Finder>,
    pub project: ProjectState,
    pub find: EditorFind,
    pub confirmation: Option<Confirmation>,
    pub context_menu: Option<ContextMenu>,
    /// `editor.toggleMinimap` for this window; `None` follows the config.
    pub minimap_override: Option<bool>,
    pub rename: Option<TextPrompt>,
    pub picker: Option<Picker>,
    /// Font zoom steps (each ±10 %) on top of `font.size`.
    pub zoom: i8,
    /// Show only the active pane of the split tree.
    pub pane_zoom: bool,
    pub process_explorer: bool,
    pub split: Option<PaneTree>,
    pub notifications: Vec<Notification>,
    /// IME composition in progress, shown in the status line.
    pub marked_text: Option<String>,
    daemon_instance: Option<u64>,
    pub permission_broker: proto_acp::PermissionBroker,
}

impl ForgeWindow {
    fn new(factory: WindowFactory, cx: &mut Context<Self>) -> Self {
        let (event_tx, events) = mpsc::channel();
        Self::spawn_housekeeping(events, cx);
        let (theme, theme_name) = load_theme(&factory);
        let keymap = ShellKeymap::default().with_overrides(&factory.config.keybindings);
        // Remembered grants live next to the user's config, keyed by
        // workspace: a cloned repository can never bring its own.
        let permission_store = factory
            .config_path
            .as_deref()
            .and_then(Path::parent)
            .map(Path::to_path_buf);
        let permission_broker =
            proto_acp::PermissionBroker::new(&factory.cwd, permission_store.as_deref());
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
            search: SearchState::default(),
            finder: None,
            project: ProjectState::default(),
            find: EditorFind::default(),
            confirmation: None,
            context_menu: None,
            minimap_override: None,
            rename: None,
            picker: None,
            zoom: 0,
            pane_zoom: false,
            process_explorer: false,
            split: None,
            notifications: Vec::new(),
            marked_text: None,
            daemon_instance: None,
            permission_broker,
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
                match this.update(cx, |view, _| {
                    view.refresh_git_diffs();
                    view.poll_project_search() | view.poll_syntax()
                }) {
                    Ok(dirty) => changed |= dirty,
                    Err(_) => return,
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
        self.refresh_search_if_due();
        dirty |= self.poll_watcher(cx);
        dirty |= self.autosave();
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
        self.keymap = ShellKeymap::default().with_overrides(&config.keybindings);
        self.factory.sources = sources;
        self.factory.config = Arc::new(config);
        self.config = Arc::clone(&self.factory.config);
        let (theme, name) = load_theme(&self.factory);
        self.set_theme(theme, name);
        self.apply_zoom(cx);
        self.notify_user(NotificationLevel::Info, "Configuración recargada");
        cx.notify();
    }

    fn set_theme(&mut self, theme: ThemeColors, name: String) {
        self.theme = theme;
        self.theme_name = name;
        let palette = Palette::from(&theme);
        for tab in self.tabs.iter_mut().filter_map(Tab::terminal_mut) {
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
            sessions: self
                .tabs
                .iter()
                .map(|tab| tab.terminal().and_then(|terminal| terminal.session_id))
                .collect(),
            daemon_instance: self.daemon_instance,
            titles: self
                .tabs
                .iter()
                .map(|tab| tab.custom_title.clone())
                .collect(),
            zoom: self.zoom,
            files: self
                .tabs
                .iter()
                .map(|tab| {
                    tab.editor()
                        .and_then(|editor| editor.path().map(Path::to_path_buf))
                })
                .collect(),
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
            match session.file(index) {
                Some(path) if path.is_file() => self.open_file(path, None, None, cx),
                _ => {
                    self.open_tab(
                        None,
                        session.daemon_session(index),
                        session.daemon_instance,
                        cx,
                    );
                }
            }
            if let Some(tab) = self.tabs.get_mut(index) {
                tab.custom_title = session.title(index).map(str::to_owned);
            }
        }
        self.active_tab = session.active.min(self.tabs.len().saturating_sub(1));
        self.split = session.split;
        if session.zoom != 0 {
            self.zoom = session.zoom;
            self.apply_zoom(cx);
        }
        if let Some(name) = session.theme
            && !name.eq_ignore_ascii_case(&self.theme_name)
            && let Ok(colors) = theme::load_theme(&name, self.themes_dir().as_deref())
        {
            self.set_theme(colors.with_overrides(&self.config.colors), name);
        }
        cx.notify();
    }

    // ----- tabs and panes -----------------------------------------------

    pub fn active_tab(&self) -> &Tab {
        &self.tabs[self.active_tab]
    }

    pub fn active_tab_mut(&mut self) -> &mut Tab {
        &mut self.tabs[self.active_tab]
    }

    /// The active tab's terminal, if it is one.
    pub fn active_terminal(&self) -> Option<&TerminalTab> {
        self.active_tab().terminal()
    }

    pub fn active_terminal_mut(&mut self) -> Option<&mut TerminalTab> {
        self.active_tab_mut().terminal_mut()
    }

    fn active_agent_mut(&mut self) -> Option<&mut AgentTab> {
        match &mut self.active_tab_mut().content {
            TabContent::Agent(agent) => Some(agent),
            TabContent::Terminal(_) | TabContent::Editor(_) => None,
        }
    }

    pub fn tab(&self, id: u64) -> Option<&Tab> {
        self.tabs.iter().find(|tab| tab.id == id)
    }

    pub(crate) fn tab_mut(&mut self, id: u64) -> Option<&mut Tab> {
        self.tabs.iter_mut().find(|tab| tab.id == id)
    }

    fn terminal_mut(&mut self, id: u64) -> Option<&mut TerminalTab> {
        self.tab_mut(id).and_then(Tab::terminal_mut)
    }

    /// Surface of pane `index`, called from the grid element's paint with the
    /// area it fills; the PTY is resized when that area changes.
    ///
    /// # Panics
    ///
    /// If pane `index` is not a terminal; the chrome only builds grid
    /// elements for terminal tabs.
    pub fn pane_surface(&mut self, index: usize, bounds: Bounds<Pixels>) -> &mut TerminalSurface {
        let (cols, rows) = crate::grid_dimensions(bounds.size, self.factory.metrics, 0.0);
        let tab = self.tabs[index]
            .terminal_mut()
            .expect("grid element painted for a terminal tab");
        if tab.viewport != Some((cols, rows)) {
            tab.viewport = Some((cols, rows));
            let _ = tab.input.send(IpcCommand::Resize { cols, rows });
        }
        &mut tab.terminal
    }

    /// Appends a tab and makes it active.
    pub fn push_tab(&mut self, content: TabContent, cx: &mut Context<Self>) -> usize {
        let id = self.next_tab_id;
        self.next_tab_id += 1;
        self.tabs.push(Tab {
            id,
            custom_title: None,
            content,
        });
        self.active_tab = self.tabs.len() - 1;
        cx.notify();
        self.active_tab
    }

    /// Creates a tab (and its daemon session) and makes it active. Without an
    /// explicit directory the new shell starts where the active one is (OSC 7).
    pub fn create_terminal_tab(&mut self, cwd: Option<PathBuf>, cx: &mut Context<Self>) -> usize {
        self.open_tab(cwd, None, None, cx)
    }

    fn create_agent_tab(&mut self, cx: &mut Context<Self>) -> usize {
        let context = self.agent_prompt_context();
        let configured = self
            .config
            .agents
            .iter()
            .find(|agent| agent.enabled)
            .cloned();
        let name = configured
            .as_ref()
            .map_or_else(|| "ACP".to_owned(), |agent| agent.name.clone());
        let mut agent_tab = AgentTab::new(name, self.factory.cwd.clone());
        agent_tab.set_context(context);
        let index = self.push_tab(TabContent::Agent(Box::new(agent_tab)), cx);
        let definition = configured.map(|configured| proto_acp::AgentDefinition {
            name: configured.name,
            command: configured.command,
            args: configured.args,
            env: configured.env,
            cwd: Some(self.factory.cwd.clone()),
            auth_method: configured.auth_method,
            enabled: configured.enabled,
        });
        let registry_cache = self
            .factory
            .config_path
            .as_ref()
            .and_then(|path| path.parent())
            .map_or_else(
                || std::env::temp_dir().join("forge-acp-registry.json"),
                |directory| directory.join("acp-registry.json"),
            );
        let (commands, command_rx) = async_mpsc::unbounded_channel();
        self.tabs[index]
            .agent_mut()
            .expect("new agent tab")
            .connect(commands);
        spawn_agent_worker(
            definition,
            registry_cache,
            self.factory.cwd.clone(),
            self.tabs[index].id,
            self.event_tx.clone(),
            command_rx,
        );
        index
    }

    fn agent_prompt_context(&self) -> Vec<PromptContext> {
        match &self.active_tab().content {
            TabContent::Editor(editor) => {
                let Some(path) = editor.path() else {
                    return Vec::new();
                };
                let selection = editor.buffer.selections().primary();
                let range = selection.range();
                let content = if selection.is_empty() {
                    editor.buffer.text()
                } else {
                    editor.buffer.slice(range.clone())
                };
                let start = editor.buffer.position_of(range.start);
                let end = editor.buffer.position_of(range.end);
                vec![PromptContext {
                    label: format!(
                        "{}:{}:{}-{}:{}",
                        path.display(),
                        start.line + 1,
                        start.column + 1,
                        end.line + 1,
                        end.column + 1
                    ),
                    content,
                }]
            }
            TabContent::Terminal(terminal) => {
                let content = terminal.terminal.selection.map_or_else(
                    || {
                        let (_, rows) = terminal.terminal.grid.dimensions();
                        (0..rows)
                            .filter_map(|row| terminal.terminal.grid.row_text(row))
                            .collect::<Vec<_>>()
                            .join("\n")
                            .trim_end()
                            .to_owned()
                    },
                    |selection| terminal.terminal.grid.selected_text(selection),
                );
                (!content.is_empty())
                    .then(|| PromptContext {
                        label: format!(
                            "terminal://{}",
                            terminal
                                .session_id
                                .map_or_else(|| "active".into(), |id| id.to_string())
                        ),
                        content,
                    })
                    .into_iter()
                    .collect()
            }
            TabContent::Agent(_) => Vec::new(),
        }
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
        self.open_tab_with(cwd, attach, daemon_instance, None, cx)
    }

    fn open_tab_with(
        &mut self,
        cwd: Option<PathBuf>,
        attach: Option<u64>,
        daemon_instance: Option<u64>,
        profile: Option<&TerminalProfile>,
        cx: &mut Context<Self>,
    ) -> usize {
        let cwd = cwd
            .or_else(|| profile.and_then(|profile| profile.cwd.clone()))
            .filter(|path| path.is_dir());
        let cwd = cwd.or_else(|| {
            self.tabs
                .get(self.active_tab)
                .and_then(Tab::terminal)
                .and_then(|tab| tab.info.pwd.clone())
                .filter(|path| path.is_dir())
        });
        let id = self.next_tab_id;
        let (input, input_rx) = async_mpsc::unbounded_channel();
        let title = profile.map_or_else(
            || {
                cwd.as_ref().and_then(|path| path.file_name()).map_or_else(
                    || format!("Terminal {id}"),
                    |name| name.to_string_lossy().into_owned(),
                )
            },
            |profile| profile.name.clone(),
        );
        let chrome_height = chrome::TOPBAR_HEIGHT + chrome::STATUS_HEIGHT;
        let (initial_cols, initial_rows) = crate::grid_dimensions(
            self.factory.window_size,
            self.factory.metrics,
            chrome_height,
        );
        let terminal = TerminalTab {
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
            clipboard_allowed: false,
            drag_anchor: None,
            viewport: None,
            agent_owner: None,
            agent_output: String::new(),
            agent_output_limit: 1_048_576,
            agent_output_truncated: false,
            agent_exited: false,
            agent_exit_code: None,
            agent_waiters: Vec::new(),
        };
        self.push_tab(TabContent::Terminal(Box::new(terminal)), cx);
        if self.factory.start_ipc {
            let cwd = cwd.unwrap_or_else(|| self.factory.cwd.clone());
            spawn_ipc_worker(
                self.factory.spec(cwd, attach, daemon_instance, profile),
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
            if self.search.open {
                self.retarget_search();
            }
            cx.notify();
        }
    }

    /// Closes a tab, asking first when it is an editor with unsaved changes.
    pub fn request_close_tab(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let dirty = self
            .tabs
            .get(index)
            .and_then(Tab::editor)
            .is_some_and(|editor| editor.buffer.is_dirty());
        if !dirty {
            self.close_tab(index, cx);
            return;
        }
        let tab_id = self.tabs[index].id;
        let title = self.tabs[index].title().into_owned();
        let answer = window.prompt(
            PromptLevel::Warning,
            &format!("¿Cerrar «{title}» sin guardar?"),
            Some(
                "Los cambios se perderán; el journal conserva una copia hasta el próximo guardado.",
            ),
            &["Guardar y cerrar", "Descartar", "Cancelar"],
            cx,
        );
        cx.spawn(async move |this, cx| {
            let choice = answer.await.unwrap_or(2);
            if choice == 2 {
                return;
            }
            let _ = this.update(cx, |view, cx| {
                let Some(index) = view.tabs.iter().position(|tab| tab.id == tab_id) else {
                    return;
                };
                if choice == 0 {
                    view.activate_tab(index, cx);
                    view.save_active(cx);
                    let still_dirty = view.tabs[index]
                        .editor()
                        .is_some_and(|editor| editor.buffer.is_dirty());
                    if still_dirty {
                        return;
                    }
                }
                view.close_tab(index, cx);
            });
        })
        .detach();
    }

    pub fn close_tab(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.tabs.len() <= 1 || index >= self.tabs.len() {
            return;
        }
        // Closing a tab ends its shell; sessions only survive when the
        // window itself goes away.
        let closed = self.tabs.remove(index);
        if let Some(terminal) = closed.terminal() {
            let _ = terminal.input.send(IpcCommand::Shutdown);
        }
        if let Some(path) = closed.editor().and_then(EditorTab::path) {
            let path = path.to_path_buf();
            self.unwatch_file(&path);
        }
        self.active_tab = if self.active_tab > index {
            self.active_tab - 1
        } else {
            self.active_tab.min(self.tabs.len() - 1)
        };
        self.split = self.split.take().and_then(|tree| tree.remove(index));
        if self.search.open {
            self.retarget_search();
        }
        cx.notify();
    }

    // ----- scrollback search --------------------------------------------

    fn open_search(&mut self, cx: &mut Context<Self>) {
        self.search.open = true;
        self.palette.open = false;
        self.retarget_search();
        cx.notify();
    }

    fn close_search(&mut self, cx: &mut Context<Self>) {
        self.search.open = false;
        self.search.clear_results();
        for tab in self.tabs.iter_mut().filter_map(Tab::terminal_mut) {
            tab.terminal.search = SearchHighlights::default();
        }
        cx.notify();
    }

    /// Points the search at the active tab and re-runs the query there.
    fn retarget_search(&mut self) {
        let active = self.active_tab().id;
        if self.search.tab_id != Some(active) {
            for tab in self.tabs.iter_mut().filter_map(Tab::terminal_mut) {
                tab.terminal.search = SearchHighlights::default();
            }
            self.search.tab_id = Some(active);
            self.search.clear_results();
        }
        self.submit_search();
    }

    /// Sends the current query to the daemon; an empty query clears.
    fn submit_search(&mut self) {
        let request = self.search.begin_request();
        let query = self.search.query.clone();
        let options = self.search.options;
        let Some(tab) = self.search.tab_id.and_then(|id| self.terminal_mut(id)) else {
            return;
        };
        match request {
            Some(request_id) => {
                let _ = tab.input.send(IpcCommand::Search {
                    request_id,
                    query,
                    regex: options.regex,
                    case_sensitive: options.case_sensitive,
                });
            }
            None => tab.terminal.search = SearchHighlights::default(),
        }
    }

    /// Toggle buttons in the bar changed a flag.
    pub fn resubmit_search(&mut self, cx: &mut Context<Self>) {
        self.submit_search();
        cx.notify();
    }

    pub fn close_search_click(&mut self, cx: &mut Context<Self>) {
        self.close_search(cx);
    }

    fn refresh_search_if_due(&mut self) {
        if self.search.refresh_due() {
            self.submit_search();
        }
    }

    fn on_search_results(
        &mut self,
        tab_id: u64,
        request_id: u64,
        matches: Vec<proto_ipc::SearchMatch>,
        error: Option<String>,
    ) {
        if self.search.tab_id != Some(tab_id) {
            return;
        }
        let had_selection = self.search.current.is_some();
        if !self.search.apply_results(request_id, matches, error) {
            return;
        }
        self.apply_search_highlights();
        // The first answer to a query jumps to its newest match; refreshes
        // after new output leave the viewport where the user put it.
        if !had_selection {
            self.reveal_current_match();
        }
        if self.search.refresh_due() {
            self.submit_search();
        }
    }

    fn apply_search_highlights(&mut self) {
        let highlights = self.search.highlights();
        if let Some(tab) = self.search.tab_id.and_then(|id| self.terminal_mut(id)) {
            tab.terminal.search = highlights;
        }
    }

    fn reveal_current_match(&mut self) {
        let Some(row) = self.search.current_row() else {
            return;
        };
        let Some(tab) = self.search.tab_id.and_then(|id| self.terminal_mut(id)) else {
            return;
        };
        if let Some(top) = reveal_row(row, tab.terminal.grid.viewport()) {
            let _ = tab.input.send(IpcCommand::Scroll(ScrollRequest::Row(top)));
        }
    }

    fn search_step(&mut self, direction: SearchDirection, cx: &mut Context<Self>) {
        if !self.search.open {
            self.open_search(cx);
        }
        if self.search.step(direction) == SearchAction::Reveal {
            self.apply_search_highlights();
            self.reveal_current_match();
        }
        cx.notify();
    }

    /// Keys while the search bar has focus. Shell chords still work so the
    /// user can open a tab or the palette without closing the search.
    fn search_key(
        &mut self,
        key: &str,
        key_char: Option<&str>,
        modifiers: KeyMods,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let plain = !modifiers.control && !modifiers.alt;
        match key {
            "escape" => return self.close_search(cx),
            "enter" | "up" if plain && !(key == "enter" && modifiers.shift) => {
                return self.search_step(SearchDirection::Older, cx);
            }
            "enter" | "down" if plain => return self.search_step(SearchDirection::Newer, cx),
            "backspace" if plain => {
                self.search.query.pop();
                self.submit_search();
            }
            "r" if modifiers.alt => {
                self.search.options.regex = !self.search.options.regex;
                self.submit_search();
            }
            "c" if modifiers.alt => {
                self.search.options.case_sensitive = !self.search.options.case_sensitive;
                self.submit_search();
            }
            "v" if modifiers.control => {
                if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                    self.search
                        .query
                        .push_str(text.lines().next().unwrap_or_default());
                    self.submit_search();
                }
            }
            _ => {
                let shell_key =
                    ShellKeystroke::new(key, modifiers.control, modifiers.alt, modifiers.shift);
                if let Some(command) = self.keymap.resolve(&shell_key, ShellContext::Terminal) {
                    return self.run_shell_command(command, window, cx);
                }
                if plain && let Some(text) = key_char {
                    self.search.query.push_str(text);
                    self.submit_search();
                }
            }
        }
        cx.notify();
    }

    /// The split tree to render: the stored one when it still contains the
    /// active tab, otherwise the active tab alone.
    pub fn visible_tree(&self) -> PaneTree {
        self.split
            .clone()
            .filter(|tree| !self.pane_zoom && tree.contains(self.active_tab))
            .unwrap_or(PaneTree::leaf(self.active_tab))
    }

    /// Swaps the active tab with its neighbour; panes follow their tabs.
    fn move_active_tab(&mut self, delta: isize, cx: &mut Context<Self>) {
        let from = self.active_tab;
        let Some(to) = from
            .checked_add_signed(delta)
            .filter(|to| *to < self.tabs.len())
        else {
            return;
        };
        self.tabs.swap(from, to);
        if let Some(tree) = &mut self.split {
            tree.swap_indices(from, to);
        }
        self.active_tab = to;
        cx.notify();
    }

    // ----- zoom -----------------------------------------------------------

    fn zoom_by(&mut self, delta: i8, cx: &mut Context<Self>) {
        let zoom = if delta == 0 {
            0
        } else {
            (self.zoom + delta).clamp(-8, 12)
        };
        if zoom == self.zoom {
            return;
        }
        self.zoom = zoom;
        self.apply_zoom(cx);
        let percent = (zoom_factor(zoom) * 100.0).round();
        self.notify_user(NotificationLevel::Info, format!("Zoom {percent} %"));
        cx.notify();
    }

    /// Recomputes cell metrics from `font.size × factor` and lets every pane
    /// resize its PTY on the next paint.
    fn apply_zoom(&mut self, cx: &mut Context<Self>) {
        let mut config = (*self.config).clone();
        config.font.size = (config.font.size * zoom_factor(self.zoom)).clamp(4.0, 200.0);
        let metrics = crate::cell_metrics_for(&config, cx);
        self.factory.metrics = metrics;
        for tab in self.tabs.iter_mut().filter_map(Tab::terminal_mut) {
            tab.terminal.metrics = metrics;
            tab.viewport = None;
        }
    }

    // ----- rename and profile picker --------------------------------------

    fn rename_key(
        &mut self,
        key: &str,
        key_char: Option<&str>,
        modifiers: KeyMods,
        cx: &mut Context<Self>,
    ) {
        let Some(prompt) = &mut self.rename else {
            return;
        };
        match key {
            "escape" => self.rename = None,
            "enter" => {
                let value = prompt.value.trim().to_owned();
                self.rename = None;
                self.active_tab_mut().custom_title = (!value.is_empty()).then_some(value);
                self.save_session();
            }
            "backspace" => {
                prompt.value.pop();
            }
            _ if !modifiers.control && !modifiers.alt => {
                if let Some(text) = key_char {
                    prompt.value.push_str(text);
                }
            }
            _ => {}
        }
        cx.notify();
    }

    fn picker_key(&mut self, key: &str, cx: &mut Context<Self>) {
        let Some(picker) = &mut self.picker else {
            return;
        };
        match key {
            "escape" => self.picker = None,
            "up" => picker.index = picker.index.saturating_sub(1),
            "down" => picker.index = (picker.index + 1).min(picker.items.len().saturating_sub(1)),
            "enter" => {
                let index = picker.index;
                self.picker = None;
                if let Some(profile) = self.config.profiles.get(index).cloned() {
                    self.open_tab_with(None, None, None, Some(&profile), cx);
                }
            }
            _ => {}
        }
        cx.notify();
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

    #[allow(clippy::too_many_lines)]
    fn handle_event(&mut self, event: UiEvent, cx: &mut Context<Self>) {
        match event {
            UiEvent::Message {
                tab_id,
                message: ServerMessage::Exited { exit_code, .. },
            } => {
                if self
                    .terminal_mut(tab_id)
                    .is_some_and(|terminal| terminal.agent_owner.is_some())
                {
                    if let Some(terminal) = self.terminal_mut(tab_id) {
                        terminal.agent_exited = true;
                        terminal.agent_exit_code = exit_code;
                        terminal.status = exit_code.map_or_else(
                            || "Proceso del agente terminado".into(),
                            |code| format!("Proceso del agente terminó con código {code}"),
                        );
                        for (id, waiter) in terminal.agent_waiters.drain(..) {
                            let _ = waiter.send(proto_acp::JsonRpcMessage::response(
                                id,
                                serde_json::json!({"exitCode": exit_code}),
                            ));
                        }
                    }
                    cx.notify();
                    return;
                }
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
                if let Some(tab) = self.terminal_mut(tab_id) {
                    tab.status = format!("Error del daemon: {message}");
                }
            }
            UiEvent::Message {
                tab_id,
                message: ServerMessage::Output { session_id, data },
            } => {
                if let Some(tab) = self.terminal_mut(tab_id)
                    && tab.agent_owner.is_some()
                {
                    tab.agent_output_truncated |=
                        append_bounded_output(&mut tab.agent_output, &data, tab.agent_output_limit);
                }
                self.apply_screen_message(tab_id, ServerMessage::Output { session_id, data });
            }
            UiEvent::Message {
                tab_id,
                message:
                    ServerMessage::SessionInfo {
                        title,
                        pwd,
                        mouse_tracking,
                        alternate_screen,
                        bracketed_paste,
                        ..
                    },
            } => {
                if let Some(tab) = self.terminal_mut(tab_id) {
                    tab.info = SessionInfo {
                        title,
                        pwd: pwd.map(PathBuf::from),
                        mouse_tracking,
                        alternate_screen,
                        bracketed_paste,
                    };
                }
            }
            UiEvent::Message {
                tab_id,
                message:
                    ServerMessage::ClipboardWrite {
                        target,
                        text,
                        program,
                        ..
                    },
            } => self.on_clipboard_write(tab_id, target, text, &program, cx),
            UiEvent::Message {
                tab_id,
                message:
                    ServerMessage::SearchResults {
                        request_id,
                        matches,
                        error,
                        ..
                    },
            } => self.on_search_results(tab_id, request_id, matches, error),
            UiEvent::Message { tab_id, message } => self.apply_screen_message(tab_id, message),
            UiEvent::Status { tab_id, status } => {
                if let Some(tab) = self.terminal_mut(tab_id) {
                    tab.status = status;
                }
            }
            UiEvent::IndexReady { index } => self.on_index_ready(*index, cx),
            UiEvent::GitDiff {
                tab_id,
                version,
                state,
            } => self.on_git_diff(tab_id, version, *state, cx),
            UiEvent::Attached {
                tab_id,
                session_id,
                daemon_instance,
            } => {
                self.daemon_instance = Some(daemon_instance);
                if let Some(tab) = self.terminal_mut(tab_id) {
                    tab.session_id = Some(session_id);
                }
                self.save_session();
            }
            UiEvent::AgentConnected { tab_id, session_id } => {
                self.connect_agent(tab_id, session_id);
            }
            UiEvent::AgentEvent { tab_id, event } => self.handle_agent_event(tab_id, event),
            UiEvent::AgentStatus { tab_id, status } => self.set_agent_status(tab_id, status),
            UiEvent::AgentRequest {
                tab_id,
                message,
                response,
            } => self.handle_agent_request(tab_id, &message, response, cx),
        }
    }

    fn handle_agent_request(
        &mut self,
        agent_id: u64,
        message: &proto_acp::JsonRpcMessage,
        response: oneshot::Sender<proto_acp::JsonRpcMessage>,
        cx: &mut Context<Self>,
    ) {
        let Some(id) = message.id.clone() else { return };
        let params = message.params.as_ref().unwrap_or(&serde_json::Value::Null);
        let result = match message.method.as_deref() {
            Some("session/request_permission") => {
                self.acp_request_permission(agent_id, id, params, response, cx);
                return;
            }
            Some("fs/read_text_file") => self.acp_read_file(params),
            Some("fs/write_text_file") => self.acp_propose_file(agent_id, &id, params),
            Some("terminal/create") => self.acp_terminal_create(agent_id, id.clone(), params, cx),
            Some("terminal/output") => self.acp_terminal_output(agent_id, id.clone(), params),
            Some("terminal/kill") => self.acp_terminal_signal(agent_id, id.clone(), params),
            Some("terminal/release") => self.acp_terminal_release(agent_id, id.clone(), params, cx),
            Some("terminal/wait_for_exit") => {
                let Some(terminal) = self.acp_terminal_mut(agent_id, params) else {
                    let _ = response.send(proto_acp::JsonRpcMessage::error(
                        Some(id),
                        -32602,
                        "terminalId inválido",
                    ));
                    return;
                };
                if terminal.agent_exited {
                    let exit_code = terminal.agent_exit_code;
                    Ok(serde_json::json!({"exitCode": exit_code}))
                } else {
                    terminal.agent_waiters.push((id, response));
                    return;
                }
            }
            Some(method) => Err(format!("método ACP no soportado: {method}")),
            None => Err("solicitud ACP sin método".into()),
        };
        let reply = match result {
            Ok(value) => proto_acp::JsonRpcMessage::response(id, value),
            Err(error) => proto_acp::JsonRpcMessage::error(Some(id), -32602, error),
        };
        let _ = response.send(reply);
        cx.notify();
    }

    fn acp_workspace_path(&self, params: &serde_json::Value) -> Result<PathBuf, String> {
        let path = params
            .get("path")
            .and_then(serde_json::Value::as_str)
            .map(PathBuf::from)
            .ok_or_else(|| "se requiere path".to_owned())?;
        if !path.is_absolute() {
            return Err("path debe ser absoluto".into());
        }
        let workspace = self
            .factory
            .cwd
            .canonicalize()
            .map_err(|error| error.to_string())?;
        let checked = if path.exists() {
            path.canonicalize().map_err(|error| error.to_string())?
        } else {
            let parent = path
                .parent()
                .ok_or_else(|| "path sin directorio padre".to_owned())?
                .canonicalize()
                .map_err(|error| error.to_string())?;
            parent.join(
                path.file_name()
                    .ok_or_else(|| "path sin nombre".to_owned())?,
            )
        };
        checked
            .starts_with(&workspace)
            .then_some(checked)
            .ok_or_else(|| "path está fuera del workspace".into())
    }

    fn acp_read_file(&self, params: &serde_json::Value) -> Result<serde_json::Value, String> {
        let path = self.acp_workspace_path(params)?;
        let content = self
            .tabs
            .iter()
            .filter_map(Tab::editor)
            .find_map(|editor| {
                editor
                    .path()
                    .and_then(|open| open.canonicalize().ok())
                    .filter(|open| open == &path)
                    .map(|_| editor.buffer.text())
            })
            .or_else(|| std::fs::read_to_string(&path).ok())
            .ok_or_else(|| format!("no se pudo leer {}", path.display()))?;
        let line = params
            .get("line")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(1);
        let limit = params
            .get("limit")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(u64::MAX);
        let start = usize::try_from(line.saturating_sub(1)).unwrap_or(usize::MAX);
        let count = usize::try_from(limit).unwrap_or(usize::MAX);
        let selected = content
            .lines()
            .skip(start)
            .take(count)
            .collect::<Vec<_>>()
            .join("\n");
        Ok(serde_json::json!({"content": selected}))
    }

    fn parse_permission_options(
        raw_options: &[serde_json::Value],
    ) -> Vec<crate::agent::PendingPermissionOption> {
        let mut options = Vec::new();
        for opt in raw_options {
            if let Some(opt_id) = opt
                .get("optionId")
                .or_else(|| opt.get("id"))
                .and_then(serde_json::Value::as_str)
            {
                let name = opt
                    .get("name")
                    .or_else(|| opt.get("title"))
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or(opt_id);
                let kind = opt
                    .get("kind")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("allow");
                options.push(crate::agent::PendingPermissionOption {
                    id: opt_id.to_owned(),
                    name: name.to_owned(),
                    kind: kind.to_owned(),
                });
            }
        }
        if options.is_empty() {
            vec![
                crate::agent::PendingPermissionOption {
                    id: "allow_once".into(),
                    name: "Permitir una vez".into(),
                    kind: "allow_once".into(),
                },
                crate::agent::PendingPermissionOption {
                    id: "allow_session".into(),
                    name: "Permitir en esta sesión".into(),
                    kind: "allow_always".into(),
                },
                crate::agent::PendingPermissionOption {
                    id: "allow_always".into(),
                    name: "Permitir siempre".into(),
                    kind: "allow_always".into(),
                },
                crate::agent::PendingPermissionOption {
                    id: "deny".into(),
                    name: "Rechazar".into(),
                    kind: "deny".into(),
                },
            ]
        } else {
            options
        }
    }

    fn send_automated_permission_response(
        id: serde_json::Value,
        raw_options: &[serde_json::Value],
        decision: proto_acp::PermissionDecision,
        response: oneshot::Sender<proto_acp::JsonRpcMessage>,
    ) {
        let option_id =
            proto_acp::select_option_id(raw_options, decision, proto_acp::PermissionTtl::Session);
        let reply =
            proto_acp::JsonRpcMessage::response(id, proto_acp::permission_outcome(option_id));
        let _ = response.send(reply);
    }

    fn acp_request_permission(
        &mut self,
        agent_id: u64,
        id: serde_json::Value,
        params: &serde_json::Value,
        response: oneshot::Sender<proto_acp::JsonRpcMessage>,
        cx: &mut Context<Self>,
    ) {
        let agent_name = self
            .tab(agent_id)
            .and_then(Tab::agent)
            .map_or_else(|| "Agente".to_owned(), |a| a.agent_name.clone());

        let tool_call = params.get("toolCall").unwrap_or(params);
        let tool_name = tool_call
            .get("name")
            .or_else(|| tool_call.get("title"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("herramienta")
            .to_owned();
        let title = tool_call
            .get("title")
            .or_else(|| tool_call.get("name"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or(&tool_name)
            .to_owned();
        let detail = tool_call
            .get("detail")
            .or_else(|| tool_call.get("content"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_owned();

        let scope = tool_call
            .get("arguments")
            .and_then(|args| {
                args.get("command")
                    .or_else(|| args.get("path"))
                    .and_then(serde_json::Value::as_str)
            })
            .or_else(|| tool_call.get("command").and_then(serde_json::Value::as_str))
            .unwrap_or(&detail)
            .to_owned();

        let kind = tool_call.get("kind").and_then(serde_json::Value::as_str);
        let capability = proto_acp::PermissionCapability::from_tool_call(kind, &tool_name);
        let decision = self
            .permission_broker
            .evaluate(&agent_name, &capability, &scope);

        let raw_options: Vec<serde_json::Value> = params
            .get("options")
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_default();

        match decision {
            proto_acp::PermissionDecision::Allow | proto_acp::PermissionDecision::Deny => {
                Self::send_automated_permission_response(id, &raw_options, decision, response);
            }
            proto_acp::PermissionDecision::Ask => {
                let options = Self::parse_permission_options(&raw_options);
                if let Some(agent) = self.tab_mut(agent_id).and_then(Tab::agent_mut) {
                    agent.timeline.push(crate::agent::TimelineItem::ToolCall {
                        id: format!("perm-{id}"),
                        title: format!("Permiso requerido · {title}"),
                        state: crate::agent::ToolState::WaitingPermission,
                        detail: if scope.is_empty() {
                            format!("{tool_name} solicita autorización")
                        } else {
                            format!("{tool_name} solicita autorización para: {scope}")
                        },
                    });
                    agent
                        .pending_permissions
                        .push(crate::agent::PendingPermissionRequest {
                            id,
                            tool_name,
                            title,
                            detail,
                            capability,
                            scope,
                            options,
                            raw_options,
                            response: Some(response),
                        });
                }
                cx.notify();
            }
        }
    }

    pub fn agent_resolve_permission(
        &mut self,
        agent_id: u64,
        request_id: &serde_json::Value,
        decision: proto_acp::PermissionDecision,
        ttl: proto_acp::PermissionTtl,
        cx: &mut Context<Self>,
    ) {
        let agent_name = self
            .tab(agent_id)
            .and_then(Tab::agent)
            .map_or_else(String::new, |a| a.agent_name.clone());
        if let Some(agent) = self.tab_mut(agent_id).and_then(Tab::agent_mut)
            && let Some((capability, scope)) = agent.resolve_permission(request_id, decision, ttl)
        {
            self.permission_broker.record_decision(
                &agent_name,
                capability,
                proto_acp::PermissionScope::CommandPattern(scope),
                decision,
                ttl,
            );
        }
        cx.notify();
    }

    fn acp_propose_file(
        &mut self,
        agent_id: u64,
        id: &serde_json::Value,
        params: &serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let path = self.acp_workspace_path(params)?;
        let proposed = params
            .get("content")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| "se requiere content".to_owned())?
            .to_owned();
        let original = std::fs::read_to_string(&path).unwrap_or_default();
        let agent_name = self
            .tab(agent_id)
            .and_then(Tab::agent)
            .map_or_else(|| "Agente".to_owned(), |a| a.agent_name.clone());

        let (proposed_edit, hunks_count) = if let Some(editor) =
            self.tabs.iter().filter_map(Tab::editor).find(|e| {
                e.path()
                    .and_then(|open| open.canonicalize().ok())
                    .as_ref()
                    .is_some_and(|open| open == &path)
            }) {
            let edit = forge_buffer::ProposedEdit::from_proposal(
                path.clone(),
                format!("Agente · {agent_name}"),
                original,
                &editor.buffer,
                proposed,
            );
            let count = edit.hunks.len();
            (edit, count)
        } else {
            let temp_buffer = forge_buffer::Buffer::new(&original);
            let edit = forge_buffer::ProposedEdit::from_proposal(
                path.clone(),
                format!("Agente · {agent_name}"),
                original,
                &temp_buffer,
                proposed,
            );
            let count = edit.hunks.len();
            (edit, count)
        };

        let agent = self
            .tab_mut(agent_id)
            .and_then(Tab::agent_mut)
            .ok_or_else(|| "la sesión agente ya no existe".to_owned())?;
        agent.proposed_edits.retain(|edit| edit.path != path);
        agent.proposed_edits.push(proposed_edit);
        agent.timeline.push(crate::agent::TimelineItem::ToolCall {
            id: format!("fs-write-{id}"),
            title: format!("Edición propuesta · {}", path.display()),
            state: crate::agent::ToolState::Succeeded,
            detail: format!("{hunks_count} hunks propuestos; pendiente de revisión"),
        });
        Ok(serde_json::json!({}))
    }

    pub fn agent_accept_hunk(
        &mut self,
        agent_id: u64,
        edit_index: usize,
        hunk_id: usize,
        cx: &mut Context<Self>,
    ) {
        let Some(agent_idx) = self.tabs.iter().position(|t| t.id == agent_id) else {
            return;
        };
        let Some(agent) = self.tabs[agent_idx].agent() else {
            return;
        };
        let Some(proposed) = agent.proposed_edits.get(edit_index) else {
            return;
        };
        // Editors store canonical paths; compare in the same space so a
        // proposal on a symlinked or relative path still finds its tab.
        let path = proposed
            .path
            .canonicalize()
            .unwrap_or_else(|_| proposed.path.clone());

        let editor_idx = self.tabs.iter().position(|t| {
            if let TabContent::Editor(e) = &t.content {
                e.path()
                    .and_then(|open| open.canonicalize().ok())
                    .as_ref()
                    .is_some_and(|open| open == &path)
            } else {
                false
            }
        });

        if let Some(editor_idx) = editor_idx {
            if agent_idx == editor_idx {
                return;
            }
            let (agent_tab, editor_tab) = if agent_idx < editor_idx {
                let (left, right) = self.tabs.split_at_mut(editor_idx);
                (&mut left[agent_idx], &mut right[0])
            } else {
                let (left, right) = self.tabs.split_at_mut(agent_idx);
                (&mut right[0], &mut left[editor_idx])
            };
            let mut failure = None;
            if let (Some(agent), Some(editor)) = (agent_tab.agent_mut(), editor_tab.editor_mut())
                && let Some(proposed) = agent.proposed_edits.get_mut(edit_index)
            {
                match proposed.apply_hunk(hunk_id, &mut editor.buffer) {
                    Ok(_) => {
                        editor.follow_cursor();
                        editor.sync_syntax();
                    }
                    Err(error) => failure = Some(error.to_string()),
                }
            }
            if let Some(error) = failure {
                self.notify_user(
                    NotificationLevel::Warning,
                    format!("No se aplicó el cambio propuesto: {error}"),
                );
            }
        } else {
            // Not open yet: open it so the edit goes through the buffer
            // (undo, journal, watcher, gutter) instead of a raw disk write,
            // then retry on the editor tab. Opening changes the active tab;
            // the agent tab is restored afterwards.
            let active = self.active_tab;
            self.open_file(&path, None, None, cx);
            let opened = self.tabs.iter().any(|tab| {
                tab.editor()
                    .and_then(|e| e.path())
                    .is_some_and(|p| p == path)
            });
            self.active_tab = active.min(self.tabs.len().saturating_sub(1));
            if opened {
                return self.agent_accept_hunk(agent_id, edit_index, hunk_id, cx);
            }
            self.notify_user(
                NotificationLevel::Error,
                format!(
                    "No se pudo abrir {} para aplicar la propuesta",
                    path.display()
                ),
            );
        }
        cx.notify();
    }

    pub fn agent_reject_hunk(
        &mut self,
        agent_id: u64,
        edit_index: usize,
        hunk_id: usize,
        cx: &mut Context<Self>,
    ) {
        if let Some(agent) = self.tab_mut(agent_id).and_then(Tab::agent_mut)
            && let Some(proposed) = agent.proposed_edits.get_mut(edit_index)
        {
            proposed.reject_hunk(hunk_id);
        }
        cx.notify();
    }

    pub fn agent_accept_all_hunks(
        &mut self,
        agent_id: u64,
        edit_index: usize,
        cx: &mut Context<Self>,
    ) {
        let Some(agent_idx) = self.tabs.iter().position(|t| t.id == agent_id) else {
            return;
        };
        let Some(agent) = self.tabs[agent_idx].agent() else {
            return;
        };
        let Some(proposed) = agent.proposed_edits.get(edit_index) else {
            return;
        };
        // Editors store canonical paths; compare in the same space so a
        // proposal on a symlinked or relative path still finds its tab.
        let path = proposed
            .path
            .canonicalize()
            .unwrap_or_else(|_| proposed.path.clone());

        let editor_idx = self.tabs.iter().position(|t| {
            if let TabContent::Editor(e) = &t.content {
                e.path()
                    .and_then(|open| open.canonicalize().ok())
                    .as_ref()
                    .is_some_and(|open| open == &path)
            } else {
                false
            }
        });

        if let Some(editor_idx) = editor_idx {
            if agent_idx == editor_idx {
                return;
            }
            let (agent_tab, editor_tab) = if agent_idx < editor_idx {
                let (left, right) = self.tabs.split_at_mut(editor_idx);
                (&mut left[agent_idx], &mut right[0])
            } else {
                let (left, right) = self.tabs.split_at_mut(agent_idx);
                (&mut right[0], &mut left[editor_idx])
            };
            let mut failure = None;
            let mut conflicts = 0;
            if let (Some(agent), Some(editor)) = (agent_tab.agent_mut(), editor_tab.editor_mut())
                && let Some(proposed) = agent.proposed_edits.get_mut(edit_index)
            {
                match proposed.apply_all_pending(&mut editor.buffer) {
                    Ok(_) => {
                        editor.follow_cursor();
                        editor.sync_syntax();
                        conflicts = proposed.conflict_count();
                    }
                    Err(error) => failure = Some(error.to_string()),
                }
            }
            if let Some(error) = failure {
                self.notify_user(
                    NotificationLevel::Warning,
                    format!("No se aplicaron los cambios propuestos: {error}"),
                );
            } else if conflicts > 0 {
                self.notify_user(
                    NotificationLevel::Warning,
                    format!(
                        "{conflicts} hunks en conflicto quedan sin aplicar; revísalos uno a uno"
                    ),
                );
            }
        } else {
            // Not open yet: open it and apply through the editor buffer.
            let active = self.active_tab;
            self.open_file(&path, None, None, cx);
            let opened = self.tabs.iter().any(|tab| {
                tab.editor()
                    .and_then(|e| e.path())
                    .is_some_and(|p| p == path)
            });
            self.active_tab = active.min(self.tabs.len().saturating_sub(1));
            if opened {
                return self.agent_accept_all_hunks(agent_id, edit_index, cx);
            }
            self.notify_user(
                NotificationLevel::Error,
                format!(
                    "No se pudo abrir {} para aplicar la propuesta",
                    path.display()
                ),
            );
        }
        cx.notify();
    }

    pub fn agent_reject_all_hunks(
        &mut self,
        agent_id: u64,
        edit_index: usize,
        cx: &mut Context<Self>,
    ) {
        if let Some(agent) = self.tab_mut(agent_id).and_then(Tab::agent_mut)
            && let Some(proposed) = agent.proposed_edits.get_mut(edit_index)
        {
            proposed.reject_all_pending();
        }
        cx.notify();
    }

    fn acp_terminal_create(
        &mut self,
        agent_id: u64,
        _id: serde_json::Value,
        params: &serde_json::Value,
        cx: &mut Context<Self>,
    ) -> Result<serde_json::Value, String> {
        let command = params
            .get("command")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| "se requiere command".to_owned())?;
        let args = params
            .get("args")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(serde_json::Value::as_str)
            .map(str::to_owned)
            .collect();
        let cwd = params
            .get("cwd")
            .and_then(serde_json::Value::as_str)
            .map(|cwd| self.acp_workspace_path(&serde_json::json!({"path": cwd})))
            .transpose()?;
        let env = params
            .get("env")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|entry| {
                Some((
                    entry.get("name")?.as_str()?.to_owned(),
                    entry.get("value")?.as_str()?.to_owned(),
                ))
            })
            .collect();
        let profile = TerminalProfile {
            name: format!("Agente · {command}"),
            shell: Some(command.to_owned()),
            args,
            cwd,
            env,
        };
        let index = self.open_tab_with(None, None, None, Some(&profile), cx);
        let terminal_id = self.tabs[index].id;
        let terminal = self.tabs[index]
            .terminal_mut()
            .expect("new ACP terminal tab");
        terminal.agent_owner = Some(agent_id);
        terminal.agent_output_limit = params
            .get("outputByteLimit")
            .and_then(serde_json::Value::as_u64)
            .and_then(|limit| usize::try_from(limit).ok())
            .unwrap_or(1_048_576);
        Ok(serde_json::json!({"terminalId": terminal_id.to_string()}))
    }

    fn acp_terminal_mut(
        &mut self,
        agent_id: u64,
        params: &serde_json::Value,
    ) -> Option<&mut TerminalTab> {
        let id = params.get("terminalId")?.as_str()?.parse::<u64>().ok()?;
        self.terminal_mut(id)
            .filter(|terminal| terminal.agent_owner == Some(agent_id))
    }

    fn acp_terminal_output(
        &mut self,
        agent_id: u64,
        _id: serde_json::Value,
        params: &serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let terminal = self
            .acp_terminal_mut(agent_id, params)
            .ok_or_else(|| "terminalId inválido".to_owned())?;
        let mut result = serde_json::json!({
            "output": terminal.agent_output,
            "truncated": terminal.agent_output_truncated,
        });
        if terminal.agent_exited {
            let exit_code = terminal.agent_exit_code;
            result["exitStatus"] = serde_json::json!({"exitCode": exit_code});
        }
        Ok(result)
    }

    fn acp_terminal_signal(
        &mut self,
        agent_id: u64,
        _id: serde_json::Value,
        params: &serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let terminal = self
            .acp_terminal_mut(agent_id, params)
            .ok_or_else(|| "terminalId inválido".to_owned())?;
        terminal
            .input
            .send(IpcCommand::Signal(ProcessSignal::Kill))
            .map_err(|_| "terminal desconectada".to_owned())?;
        Ok(serde_json::json!({}))
    }

    fn acp_terminal_release(
        &mut self,
        agent_id: u64,
        _id: serde_json::Value,
        params: &serde_json::Value,
        cx: &mut Context<Self>,
    ) -> Result<serde_json::Value, String> {
        let terminal_id = params
            .get("terminalId")
            .and_then(serde_json::Value::as_str)
            .and_then(|id| id.parse::<u64>().ok())
            .ok_or_else(|| "terminalId inválido".to_owned())?;
        let index = self
            .tabs
            .iter()
            .position(|tab| {
                tab.id == terminal_id
                    && tab
                        .terminal()
                        .is_some_and(|terminal| terminal.agent_owner == Some(agent_id))
            })
            .ok_or_else(|| "terminalId inválido".to_owned())?;
        self.close_tab(index, cx);
        Ok(serde_json::json!({}))
    }

    fn connect_agent(&mut self, tab_id: u64, session_id: String) {
        if let Some(agent) = self.tab_mut(tab_id).and_then(Tab::agent_mut) {
            agent.session_id = Some(session_id);
            agent.status = "Sesión activa".into();
        }
    }

    fn set_agent_status(&mut self, tab_id: u64, status: String) {
        if let Some(agent) = self.tab_mut(tab_id).and_then(Tab::agent_mut) {
            agent.status = status;
        }
    }

    fn handle_agent_event(&mut self, tab_id: u64, event: proto_acp::AcpEvent) {
        let Some(agent) = self.tab_mut(tab_id).and_then(Tab::agent_mut) else {
            return;
        };
        match event {
            proto_acp::AcpEvent::SessionUpdate { update, .. } => {
                agent.apply_update(&update);
            }
            proto_acp::AcpEvent::Stderr(line) => agent.status = line,
            proto_acp::AcpEvent::Notification { method, params } => {
                agent.apply_update(&serde_json::json!({"type": method, "params": params}));
            }
            proto_acp::AcpEvent::Disconnected => {
                agent.status = "ACP desconectado".into();
            }
        }
    }

    /// Applies a screen patch (or ignores an unrelated message) and keeps
    /// selection and search highlights consistent with the new rows.
    fn apply_screen_message(&mut self, tab_id: u64, message: ServerMessage) {
        let Some(tab) = self.terminal_mut(tab_id) else {
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
        let before = (
            tab.terminal.grid.dimensions(),
            tab.terminal.grid.viewport().total,
        );
        match tab.terminal.grid.apply_server_message(message) {
            Ok(true) => {
                let after = (
                    tab.terminal.grid.dimensions(),
                    tab.terminal.grid.viewport().total,
                );
                if after.0 != before.0 {
                    tab.terminal.selection = None;
                }
                tab.status = format!("Sesión activa · revisión {}", tab.terminal.grid.revision());
                // Reflow or new output moves rows; the matches are re-run
                // (throttled) so highlights stay in place.
                if after != before && self.search.open && self.search.tab_id == Some(tab_id) {
                    self.search.invalidate();
                    self.refresh_search_if_due();
                }
            }
            Ok(false) => {}
            Err(error) => tab.status = format!("Patch inválido: {error}"),
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
        if self.overlay_key(
            keystroke.key.as_str(),
            keystroke.key_char.as_deref(),
            modifiers,
            window,
            cx,
        ) {
            return;
        }
        let shell_key = ShellKeystroke::new(
            &keystroke.key,
            modifiers.control,
            modifiers.alt,
            modifiers.shift,
        );
        let context = if self.active_tab().editor().is_some() {
            ShellContext::Editor
        } else if matches!(self.active_tab().content, TabContent::Agent(_)) {
            ShellContext::Agent
        } else {
            ShellContext::Terminal
        };
        if let Some(command) = self.keymap.resolve(&shell_key, context) {
            self.run_shell_command(command, window, cx);
            return;
        }
        if context == ShellContext::Editor {
            self.editor_key(
                keystroke.key.as_str(),
                keystroke.key_char.as_deref(),
                modifiers,
                cx,
            );
            return;
        }
        if context == ShellContext::Agent {
            self.agent_key(
                keystroke.key.as_str(),
                keystroke.key_char.as_deref(),
                modifiers,
                cx,
            );
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
                cx.notify();
                return;
            }
            "insert" if scrollback => {
                self.paste(read_primary(cx));
                cx.notify();
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
        if (event.key != TerminalKey::Unidentified || event.text.is_some())
            && let Some(tab) = self.active_terminal_mut()
        {
            tab.terminal.selection = None;
            let _ = tab.input.send(IpcCommand::Key(event));
            cx.notify();
        }
    }

    fn agent_key(
        &mut self,
        key: &str,
        key_char: Option<&str>,
        modifiers: KeyMods,
        cx: &mut Context<Self>,
    ) {
        let Some(agent) = self.active_agent_mut() else {
            return;
        };
        match key {
            "escape" => {
                agent.cancel();
                agent.status = "Cancelación solicitada".into();
            }
            "enter" if !modifiers.shift => {
                let _ = agent.submit_prompt();
            }
            "enter" => agent.prompt.push('\n'),
            "backspace" => {
                agent.prompt.pop();
            }
            "pageup" => agent.scroll_by(-agent.visible_items.cast_signed()),
            "pagedown" => agent.scroll_by(agent.visible_items.cast_signed()),
            "home" if modifiers.control => agent.scroll_item = 0,
            "end" if modifiers.control => agent.scroll_to_end(),
            _ if !modifiers.control && !modifiers.alt => {
                if let Some(text) = key_char {
                    agent.prompt.push_str(text);
                }
            }
            _ => {}
        }
        cx.notify();
    }

    /// Routes a key to whichever overlay is open (confirmation, rename,
    /// picker, palette, search bar). Returns whether one consumed it.
    fn overlay_key(
        &mut self,
        key: &str,
        key_char: Option<&str>,
        modifiers: KeyMods,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.context_menu.is_some() {
            self.context_menu_key(key, window, cx);
        } else if self.confirmation.is_some() {
            self.confirmation_key(key, cx);
        } else if self.rename.is_some() {
            self.rename_key(key, key_char, modifiers, cx);
        } else if self.picker.is_some() {
            self.picker_key(key, cx);
        } else if self.finder.is_some() {
            self.finder_key(key, key_char, modifiers, cx);
        } else if self.palette.open {
            self.palette_key(key, key_char, modifiers, window, cx);
        } else if self.search.open {
            self.search_key(key, key_char, modifiers, window, cx);
        } else if self.find.open && self.active_tab().editor().is_some() {
            self.find_key(key, key_char, modifiers, cx);
        } else {
            return false;
        }
        true
    }

    /// Rows scrolled by Shift+PageUp/PageDown: one screen minus a line.
    fn page_rows(&self) -> i64 {
        let rows = self
            .active_terminal()
            .map_or(24, |tab| tab.terminal.grid.dimensions().1);
        i64::from(rows.saturating_sub(1).max(1))
    }

    fn scroll_active(&mut self, scroll: ScrollRequest) {
        if let Some(tab) = self.active_terminal() {
            let _ = tab.input.send(IpcCommand::Scroll(scroll));
        }
    }

    /// Mouse wheel: the application gets it while it tracks the mouse;
    /// otherwise it moves the viewport over the scrollback.
    /// The visible pane under a window position. Hidden tabs keep the
    /// bounds they were last painted at, so only the panes of the current
    /// tree are candidates: otherwise a click on an editor could "hit"
    /// the terminal that used to be there.
    fn pane_at(&self, position: Point<Pixels>) -> Option<usize> {
        self.visible_tree().leaves().into_iter().find(|index| {
            self.tabs
                .get(*index)
                .is_some_and(|tab| tab.contains(position))
        })
    }

    pub fn on_scroll_wheel(&mut self, event: &ScrollWheelEvent, cx: &mut Context<Self>) {
        if matches!(self.active_tab().content, TabContent::Agent(_)) {
            let lines = match event.delta {
                ScrollDelta::Lines(delta) => delta.y * 3.0,
                ScrollDelta::Pixels(delta) => f32::from(delta.y) / self.factory.metrics.height,
            };
            #[allow(clippy::cast_possible_truncation)]
            let rows = lines.round() as isize;
            if rows != 0
                && let Some(agent) = self.active_agent_mut()
            {
                agent.scroll_by(-rows);
                cx.notify();
            }
            return;
        }
        let Some(index) = self.pane_at(event.position) else {
            return;
        };
        let lines = match event.delta {
            ScrollDelta::Lines(delta) => delta.y,
            ScrollDelta::Pixels(delta) => f32::from(delta.y) / self.factory.metrics.height,
        };
        if self.tabs[index].editor().is_some() {
            // Editors scroll three lines per notch, like VS Code; a
            // horizontal wheel (or Shift+wheel) pans long lines.
            let horizontal = match event.delta {
                ScrollDelta::Lines(delta) => delta.x * self.factory.metrics.width * 3.0,
                ScrollDelta::Pixels(delta) => f32::from(delta.x),
            };
            if event.modifiers.shift || (horizontal.abs() > 0.0 && lines.abs() == 0.0) {
                let delta = if event.modifiers.shift {
                    lines * self.factory.metrics.width * 3.0
                } else {
                    horizontal
                };
                self.activate_tab(index, cx);
                self.editor_scroll_x(-delta, cx);
                return;
            }
            #[allow(clippy::cast_possible_truncation)]
            let rows = (lines * 3.0).round() as isize;
            if rows != 0
                && let Some(editor) = self.tabs[index].editor_mut()
            {
                editor.scroll_by(-rows);
                cx.notify();
            }
            return;
        }
        #[allow(clippy::cast_possible_truncation)]
        let rows = lines.round() as i64;
        if rows == 0 {
            return;
        }
        let Some(tab) = self.tabs[index].terminal() else {
            return;
        };
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

    fn run_agent_shell_command(&mut self, command: ShellCommand, cx: &mut Context<Self>) {
        match command {
            ShellCommand::NewAgentSession => {
                self.create_agent_tab(cx);
            }
            ShellCommand::AgentAcceptAllHunks => {
                let tab_id = self.active_tab().id;
                let edits_count = self
                    .active_tab()
                    .agent()
                    .map_or(0, |a| a.proposed_edits.len());
                for edit_idx in 0..edits_count {
                    self.agent_accept_all_hunks(tab_id, edit_idx, cx);
                }
            }
            ShellCommand::AgentRejectAllHunks => {
                let tab_id = self.active_tab().id;
                let edits_count = self
                    .active_tab()
                    .agent()
                    .map_or(0, |a| a.proposed_edits.len());
                for edit_idx in 0..edits_count {
                    self.agent_reject_all_hunks(tab_id, edit_idx, cx);
                }
            }
            _ => {}
        }
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
            ShellCommand::NewAgentSession
            | ShellCommand::AgentAcceptAllHunks
            | ShellCommand::AgentRejectAllHunks => {
                self.run_agent_shell_command(command, cx);
            }
            ShellCommand::NewTerminalTabInDirectory => Self::prompt_for_directory(cx),
            ShellCommand::CloseWindow if self.tabs.len() > 1 => {
                self.request_close_tab(self.active_tab, window, cx);
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
            ShellCommand::SearchScrollback if self.search.open => self.close_search(cx),
            ShellCommand::SearchScrollback => self.open_search(cx),
            ShellCommand::SearchNext => self.search_step(SearchDirection::Older, cx),
            ShellCommand::SearchPrevious => self.search_step(SearchDirection::Newer, cx),
            ShellCommand::PreviousPrompt => {
                self.send_to_terminal(IpcCommand::ScrollToPrompt(PromptDirection::Previous));
            }
            ShellCommand::NextPrompt => {
                self.send_to_terminal(IpcCommand::ScrollToPrompt(PromptDirection::Next));
            }
            ShellCommand::SignalInterrupt => self.signal(ProcessSignal::Interrupt),
            ShellCommand::SignalTerminate => self.signal(ProcessSignal::Terminate),
            ShellCommand::SignalKill => self.signal(ProcessSignal::Kill),
            ShellCommand::OpenFile => Self::prompt_open_file(cx),
            ShellCommand::NewFile => self.new_editor_tab(cx),
            ShellCommand::SaveFile => self.save_active(cx),
            ShellCommand::EditorUndo => self.editor_undo(false, cx),
            ShellCommand::EditorRedo => self.editor_undo(true, cx),
            ShellCommand::EditorSelectAll => self.editor_select_all(cx),
            ShellCommand::EditorCopy => self.editor_copy(false, cx),
            ShellCommand::EditorCut => self.editor_copy(true, cx),
            ShellCommand::EditorPaste => self.editor_paste(cx),
            ShellCommand::EditorSelectNextMatch => self.editor_select_next_match(cx),
            ShellCommand::EditorAddCursorAbove => self.editor_add_cursor(false, cx),
            ShellCommand::EditorAddCursorBelow => self.editor_add_cursor(true, cx),
            ShellCommand::OpenProjectFile => self.open_finder(FinderMode::Files, cx),
            ShellCommand::SearchProject => self.open_finder(FinderMode::ProjectSearch, cx),
            ShellCommand::EditorFind => self.open_find(false, cx),
            ShellCommand::EditorReplace => self.open_find(true, cx),
            ShellCommand::EditorMaterialize => self.materialize_active(cx),
            ShellCommand::ToggleWordWrap => self.toggle_word_wrap(cx),
            ShellCommand::ToggleMinimap => self.toggle_minimap(cx),
            ShellCommand::ShowContextMenu => {
                let position = self
                    .active_tab()
                    .editor()
                    .and_then(|_| self.editor_cursor_bounds(window))
                    .map_or_else(
                        || point(px(120.0), px(120.0)),
                        |bounds| bounds.bottom_left(),
                    );
                self.open_context_menu(position, cx);
            }
            ShellCommand::TerminalCopy => self.copy_selection(cx),
            ShellCommand::TerminalPaste => {
                self.paste(cx.read_from_clipboard());
                cx.notify();
            }
            other => self.run_layout_command(other, cx),
        }
    }

    /// Tab, zoom and pane-layout commands, split out of
    /// [`Self::run_shell_command`] to keep each match readable.
    fn run_layout_command(&mut self, command: ShellCommand, cx: &mut Context<Self>) {
        match command {
            ShellCommand::RenameTab => {
                self.rename = Some(TextPrompt {
                    title: "Nombre de la pestaña".into(),
                    value: self.active_tab().custom_title.clone().unwrap_or_default(),
                });
                cx.notify();
            }
            ShellCommand::MoveTabLeft => self.move_active_tab(-1, cx),
            ShellCommand::MoveTabRight => self.move_active_tab(1, cx),
            ShellCommand::ZoomIn => self.zoom_by(1, cx),
            ShellCommand::ZoomOut => self.zoom_by(-1, cx),
            ShellCommand::ZoomReset => self.zoom_by(0, cx),
            ShellCommand::FocusPreviousPane => {
                let previous = self.visible_tree().previous_leaf(self.active_tab);
                let previous = if previous == self.active_tab && self.tabs.len() > 1 {
                    (self.active_tab + self.tabs.len() - 1) % self.tabs.len()
                } else {
                    previous
                };
                self.activate_tab(previous, cx);
            }
            ShellCommand::ZoomPane => {
                self.pane_zoom = !self.pane_zoom;
                cx.notify();
            }
            ShellCommand::Unsplit => {
                self.split = None;
                self.pane_zoom = false;
                cx.notify();
            }
            ShellCommand::NewTabWithProfile => {
                if self.config.profiles.is_empty() {
                    self.notify_user(
                        NotificationLevel::Info,
                        "Sin perfiles: añade [[profiles]] con name/shell/args/cwd/env en config.toml",
                    );
                } else {
                    self.picker = Some(Picker {
                        title: "Perfil de terminal".into(),
                        items: self
                            .config
                            .profiles
                            .iter()
                            .map(|profile| {
                                format!(
                                    "{} · {}",
                                    profile.name,
                                    profile.shell.as_deref().unwrap_or("shell por defecto")
                                )
                            })
                            .collect(),
                        index: 0,
                    });
                }
                cx.notify();
            }
            _ => {}
        }
    }

    /// Signals the process group behind the active tab, for programs that
    /// swallowed Ctrl+C or hung.
    fn signal(&mut self, signal: ProcessSignal) {
        if self.active_terminal().is_none() {
            return;
        }
        self.send_to_terminal(IpcCommand::Signal(signal));
        self.notify_user(
            NotificationLevel::Info,
            format!("Señal enviada: {signal:?}"),
        );
    }

    // ----- context menu ---------------------------------------------------

    /// Opens the right-click menu for the active tab at `position`.
    fn open_context_menu(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        let items = if self.active_tab().editor().is_some() {
            vec![
                ShellCommand::EditorCut,
                ShellCommand::EditorCopy,
                ShellCommand::EditorPaste,
                ShellCommand::EditorSelectAll,
                ShellCommand::EditorFind,
                ShellCommand::EditorReplace,
                ShellCommand::ToggleWordWrap,
                ShellCommand::ToggleMinimap,
                ShellCommand::SaveFile,
            ]
        } else {
            vec![
                ShellCommand::TerminalCopy,
                ShellCommand::TerminalPaste,
                ShellCommand::SearchScrollback,
                ShellCommand::NewTerminalTab,
                ShellCommand::SplitVertical,
                ShellCommand::SplitHorizontal,
                ShellCommand::RenameTab,
            ]
        };
        self.context_menu = Some(ContextMenu {
            position,
            items,
            index: 0,
        });
        cx.notify();
    }

    fn context_menu_key(&mut self, key: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(menu) = &mut self.context_menu else {
            return;
        };
        match key {
            "escape" => self.context_menu = None,
            "up" => menu.index = menu.index.saturating_sub(1),
            "down" => menu.index = (menu.index + 1).min(menu.items.len().saturating_sub(1)),
            "enter" => {
                let command = menu.items.get(menu.index).copied();
                self.context_menu = None;
                if let Some(command) = command {
                    self.run_shell_command(command, window, cx);
                }
            }
            _ => {}
        }
        cx.notify();
    }

    /// A menu row was clicked.
    pub fn context_menu_pick(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let command = self
            .context_menu
            .take()
            .and_then(|menu| menu.items.get(index).copied());
        if let Some(command) = command {
            self.run_shell_command(command, window, cx);
        }
        cx.notify();
    }

    // ----- confirmations: paste protection and OSC 52 ---------------------

    fn confirmation_key(&mut self, key: &str, cx: &mut Context<Self>) {
        let accept = matches!(key, "enter" | "y");
        let always = key == "a";
        let decline = matches!(key, "escape" | "n");
        if !(accept || always || decline) {
            return;
        }
        let Some(confirmation) = self.confirmation.take() else {
            return;
        };
        if accept || always {
            match confirmation.kind {
                ConfirmationKind::Paste(text) => self.send_paste(text),
                ConfirmationKind::Clipboard {
                    tab_id,
                    target,
                    text,
                } => {
                    if always && let Some(tab) = self.terminal_mut(tab_id) {
                        tab.clipboard_allowed = true;
                    }
                    write_clipboard(cx, target, text);
                }
            }
        }
        cx.notify();
    }

    /// OSC 52 from an application: the daemon never touches a clipboard,
    /// so the policy lives here, per tab.
    fn on_clipboard_write(
        &mut self,
        tab_id: u64,
        target: ClipboardTarget,
        text: String,
        program: &str,
        cx: &mut Context<Self>,
    ) {
        let policy = self.config.terminal.clipboard_write;
        let allowed = self
            .tabs
            .iter()
            .find(|tab| tab.id == tab_id)
            .and_then(Tab::terminal)
            .is_some_and(|tab| tab.clipboard_allowed);
        match policy {
            ClipboardPolicy::Deny => {
                self.notify_user(
                    NotificationLevel::Info,
                    "Un programa intentó escribir el portapapeles (OSC 52); denegado por configuración",
                );
            }
            ClipboardPolicy::Allow => write_clipboard(cx, target, text),
            ClipboardPolicy::Ask if allowed => write_clipboard(cx, target, text),
            ClipboardPolicy::Ask => {
                let who = if program.is_empty() {
                    "Un programa de la terminal".to_owned()
                } else {
                    format!("«{program}»")
                };
                self.confirmation = Some(Confirmation {
                    title: "¿Permitir escribir el portapapeles?".into(),
                    body: format!(
                        "{who} quiere copiar {} caracteres: {}",
                        text.chars().count(),
                        single_line(&text, 80)
                    ),
                    kind: ConfirmationKind::Clipboard {
                        tab_id,
                        target,
                        text,
                    },
                });
            }
        }
        cx.notify();
    }

    /// Sends a command to the active terminal; editors ignore it.
    fn send_to_terminal(&self, command: IpcCommand) {
        if let Some(tab) = self.active_terminal() {
            let _ = tab.input.send(command);
        }
    }

    // ----- links --------------------------------------------------------

    /// What lies under `cell` in the active tab: an OSC 8 hyperlink, else a
    /// URL or `path:line` recognised in the row text.
    fn link_at(&self, cell: CellPos) -> Option<(Range<u16>, LinkTarget)> {
        let grid = &self.active_terminal()?.terminal.grid;
        if let Some(uri) = grid.cell(cell.x, cell.y).and_then(|c| c.hyperlink.clone()) {
            let row = grid.row(cell.y)?;
            let same = |x: u16| row[usize::from(x)].hyperlink.as_deref() == Some(uri.as_str());
            let mut start = cell.x;
            while start > 0 && same(start - 1) {
                start -= 1;
            }
            let mut end = cell.x + 1;
            while usize::from(end) < row.len() && same(end) {
                end += 1;
            }
            return Some((start..end, LinkTarget::Url(uri)));
        }
        let text = grid.row_text_padded(cell.y)?;
        let link = links::link_at(&text, usize::from(cell.x))?;
        let range = u16::try_from(link.range.start).ok()?..u16::try_from(link.range.end).ok()?;
        Some((range, link.target))
    }

    fn open_link(&mut self, target: LinkTarget, cx: &mut Context<Self>) {
        match target {
            LinkTarget::Url(url) => cx.open_url(&url),
            LinkTarget::File { path, line, column } => {
                let base = self
                    .active_terminal()
                    .and_then(|tab| tab.info.pwd.clone())
                    .unwrap_or_else(|| self.factory.cwd.clone());
                let expanded = if let Some(rest) = path.strip_prefix("~/") {
                    std::env::var_os("HOME").map_or_else(
                        || PathBuf::from(&path),
                        |home| PathBuf::from(home).join(rest),
                    )
                } else {
                    PathBuf::from(&path)
                };
                let resolved = if expanded.is_absolute() {
                    expanded
                } else {
                    base.join(expanded)
                };
                if !resolved.exists() {
                    self.notify_user(
                        NotificationLevel::Warning,
                        format!("No existe {}", resolved.display()),
                    );
                    return;
                }
                // Files open in Forge's editor unless the user configured an
                // external opener; directories go to the desktop opener.
                if resolved.is_file() && self.config.terminal.open_file_command.is_empty() {
                    self.open_file(&resolved, line, column, cx);
                    return;
                }
                let (program, args) = open_file_command(
                    &self.config.terminal.open_file_command,
                    &resolved,
                    line,
                    column,
                );
                match std::process::Command::new(&program).args(&args).spawn() {
                    Ok(_) => tracing::debug!(%program, ?args, "opened file reference"),
                    Err(error) => self.notify_user(
                        NotificationLevel::Error,
                        format!("No se pudo ejecutar {program}: {error}"),
                    ),
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
        let terminal = &self.active_terminal()?.terminal;
        let text = terminal.grid.selected_text(terminal.selection?);
        (!text.is_empty()).then_some(text)
    }

    fn copy_selection(&self, cx: &mut Context<Self>) {
        if let Some(text) = self.selected_text() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    /// The daemon applies bracketed paste when the application asked for it.
    /// Multi-line text outside bracketed paste, or text that could escape
    /// the bracket, is confirmed first: a pasted newline runs a command.
    fn paste(&mut self, item: Option<ClipboardItem>) {
        let Some(text) = item.and_then(|item| item.text()) else {
            return;
        };
        let Some(bracketed) = self.active_terminal().map(|tab| tab.info.bracketed_paste) else {
            return;
        };
        match links::paste_risk(&text, bracketed) {
            None => self.send_paste(text),
            Some(risk) => {
                let lines = text.lines().count();
                let body = match risk {
                    PasteRisk::Multiline => format!(
                        "El texto tiene {lines} líneas y la aplicación no usa bracketed paste: cada salto de línea se ejecutará como Enter."
                    ),
                    PasteRisk::BracketEscape => "El texto contiene la secuencia de fin de bracketed paste (ESC [201~), que puede inyectar comandos.".into(),
                };
                self.confirmation = Some(Confirmation {
                    title: "¿Pegar de todas formas?".into(),
                    body,
                    kind: ConfirmationKind::Paste(text),
                });
            }
        }
    }

    fn send_paste(&self, text: String) {
        self.send_to_terminal(IpcCommand::Paste(text));
    }

    pub fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.context_menu.take().is_some() {
            cx.notify();
            if event.button == MouseButton::Left {
                // The click that dismissed the menu is not an edit click.
                return;
            }
        }
        let Some(index) = self.pane_at(event.position) else {
            return;
        };
        self.activate_tab(index, cx);
        let app_wants_mouse = self
            .active_terminal()
            .is_some_and(|tab| tab.app_wants_mouse(event.modifiers));
        if event.button == MouseButton::Right && !app_wants_mouse {
            self.open_context_menu(event.position, cx);
            return;
        }
        if self.active_tab().editor().is_some() {
            if event.button == MouseButton::Left {
                self.editor_mouse_down(event, window, cx);
            }
            return;
        }
        let Some(cell) = self
            .active_terminal()
            .and_then(|tab| tab.terminal.cell_at(event.position))
        else {
            return;
        };
        let Some(tab) = self.active_terminal_mut() else {
            return;
        };
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
        if event.modifiers.control {
            tab.drag_anchor = None;
            if let Some((_, target)) = self.link_at(cell) {
                self.open_link(target, cx);
            }
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

    pub fn on_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.active_tab().editor().is_some() {
            if event.pressed_button == Some(MouseButton::Left) {
                self.editor_mouse_drag(event.position, window, cx);
            }
            return;
        }
        let Some(tab) = self.active_terminal() else {
            return;
        };
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
        // Ctrl+hover underlines what a Ctrl+click would open.
        let hover = if event.modifiers.control && event.pressed_button.is_none() {
            tab.terminal
                .cell_at(event.position)
                .filter(|_| tab.terminal.contains(event.position))
                .and_then(|cell| self.link_at(cell).map(|(range, _)| (cell.y, range)))
        } else {
            None
        };
        let anchor = tab.drag_anchor;
        let head = tab.terminal.cell_at(event.position);
        let Some(tab) = self.active_terminal_mut() else {
            return;
        };
        if tab.terminal.hover_link != hover {
            tab.terminal.hover_link = hover;
            cx.notify();
        }
        let Some(anchor) = anchor else {
            return;
        };
        if event.pressed_button != Some(MouseButton::Left) {
            tab.drag_anchor = None;
            return;
        }
        let Some(head) = head else {
            return;
        };
        let selection = Some(Selection { anchor, head });
        if tab.terminal.selection != selection {
            tab.terminal.selection = selection;
            cx.notify();
        }
    }

    pub fn on_mouse_up(&mut self, event: &MouseUpEvent, cx: &mut Context<Self>) {
        if self.active_tab().editor().is_some() {
            self.editor_mouse_up();
            return;
        }
        let Some(tab) = self.active_terminal() else {
            return;
        };
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
        if self
            .active_terminal_mut()
            .and_then(|tab| tab.drag_anchor.take())
            .is_none()
        {
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
        if text.is_empty() {
            if had_preedit {
                cx.notify();
            }
            return;
        }
        if self.active_tab().editor().is_some() {
            self.editor_insert_text(text, cx);
            return;
        }
        if let Some(tab) = self.active_terminal_mut() {
            tab.terminal.selection = None;
            let _ = tab.input.send(IpcCommand::Key(key_event(
                "",
                Some(text),
                KeyMods::default(),
                KeyAction::Press,
            )));
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
        window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        match &self.active_tab().content {
            TabContent::Terminal(terminal) => terminal.terminal.cursor_bounds(),
            TabContent::Editor(_) => self.editor_cursor_bounds(window),
            TabContent::Agent(_) => None,
        }
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

/// 10 % per step, like browsers.
fn zoom_factor(steps: i8) -> f32 {
    1.1_f32.powi(i32::from(steps))
}

/// Writes an application-requested clipboard payload to the target it named.
fn write_clipboard(cx: &mut App, target: ClipboardTarget, text: String) {
    let item = ClipboardItem::new_string(text);
    match target {
        ClipboardTarget::Clipboard => cx.write_to_clipboard(item),
        ClipboardTarget::Primary => write_primary(cx, item),
    }
}

/// Program and arguments that open `file` at `line:column`: the configured
/// template, else the desktop opener.
fn open_file_command(
    template: &[String],
    file: &Path,
    line: Option<u32>,
    column: Option<u32>,
) -> (String, Vec<String>) {
    let file = file.to_string_lossy();
    if let Some((program, args)) = template.split_first() {
        let line = line.map_or_else(|| "1".to_owned(), |line| line.to_string());
        let column = column.map_or_else(|| "1".to_owned(), |column| column.to_string());
        let args = args
            .iter()
            .map(|arg| {
                arg.replace("{file}", &file)
                    .replace("{line}", &line)
                    .replace("{column}", &column)
            })
            .collect();
        return (program.clone(), args);
    }
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(windows) {
        "explorer"
    } else {
        "xdg-open"
    };
    (opener.into(), vec![file.into_owned()])
}

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

fn append_bounded_output(output: &mut String, bytes: &[u8], limit: usize) -> bool {
    output.push_str(&String::from_utf8_lossy(bytes));
    if output.len() <= limit {
        return false;
    }
    let mut start = output.len() - limit;
    while !output.is_char_boundary(start) {
        start += 1;
    }
    output.drain(..start);
    true
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
    fn open_file_template_substitutes_placeholders() {
        let template = vec![
            "code".to_owned(),
            "-g".to_owned(),
            "{file}:{line}:{column}".to_owned(),
        ];
        let (program, args) = open_file_command(&template, Path::new("/tmp/a.rs"), Some(4), None);
        assert_eq!(program, "code");
        assert_eq!(args, ["-g", "/tmp/a.rs:4:1"]);
        let (program, args) = open_file_command(&[], Path::new("/tmp/a.rs"), None, None);
        assert!(!program.is_empty());
        assert_eq!(args, ["/tmp/a.rs"]);
    }

    #[test]
    fn notifications_collapse_to_one_line() {
        assert_eq!(single_line("a\n  b\tc", 10), "a b c");
        assert_eq!(single_line("abcdefghij", 5), "abcd…");
        assert_eq!(single_line("", 5), "");
    }

    #[test]
    fn acp_terminal_output_keeps_the_newest_complete_characters() {
        let mut output = "old".to_owned();
        assert!(append_bounded_output(&mut output, "·new".as_bytes(), 4));
        assert_eq!(output, "new");
        assert!(output.len() <= 4);
    }
}
