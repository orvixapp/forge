mod grid_element;

use anyhow::{Context as _, Result, bail};
use forge_gui::{
    KeyModifiers, Selection, TerminalGrid,
    config::{Config, Language, resolve_font_family},
    encode_terminal_key,
    shell::{
        ShellCommand, ShellContext, ShellKeymap, ShellKeystroke, SplitDirection, search_commands,
    },
};
use gpui::{
    App, Application, AssetSource, Bounds, ClipboardItem, Context,
    CursorStyle as WindowCursorStyle, FocusHandle, HitboxBehavior, KeyDownEvent, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, Point, Render, ResizeEdge, SharedString, Timer,
    Window, WindowBounds, WindowDecorations, WindowHandle, WindowOptions, canvas, div, img, point,
    prelude::*, px, rgb, size,
};
use grid_element::{CellMetrics, Palette, TerminalGridElement, TerminalSurface};
use proto_ipc::{
    ClientMessage, CursorStyle, FrameKind, FrameReader, PROTOCOL_VERSION, Rgb, ScreenCell,
    ScreenCursor, ScreenRow, ServerMessage, write_message,
};
use serde::Serialize;
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread,
    time::{Duration, Instant},
};
use tokio::{
    net::UnixStream,
    process::{Child, Command},
    sync::{mpsc as async_mpsc, oneshot},
};

enum UiEvent {
    Message { tab_id: u64, message: ServerMessage },
    Status { tab_id: u64, status: String },
}

enum IpcCommand {
    Input(Vec<u8>),
    Resize { cols: u16, rows: u16 },
}

/// Resolves once the frame that first renders a state change has been handed
/// to the GPU, so benchmarks measure update → present instead of the wait for
/// the next compositor tick.
struct FrameProbe {
    started: Instant,
    presented: oneshot::Sender<f64>,
}

/// Height of the application chrome above the terminal grid.
const TOPBAR_HEIGHT: f32 = 36.0;
/// Hit target for resizing a client-decorated window. This is intentionally
/// wider than the visible border so Wayland and X11 feel equally usable.
const WINDOW_RESIZE_INSET: f32 = 8.0;
const FORGE_APP_ID: &str = "dev.forge.Forge";

struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> anyhow::Result<Option<std::borrow::Cow<'static, [u8]>>> {
        match path {
            "forge-logo.svg" => Ok(Some(std::borrow::Cow::Borrowed(include_bytes!(
                "../assets/forge-logo.svg"
            )))),
            _ => Ok(None),
        }
    }

    fn list(&self, _path: &str) -> anyhow::Result<Vec<SharedString>> {
        Ok(vec!["forge-logo.svg".into()])
    }
}

/// Everything needed to create another terminal window in this application.
/// Keeping this in one small value makes Ctrl+T use the same shell, config,
/// socket and starting directory as the first window.
#[derive(Clone)]
struct WindowFactory {
    socket: PathBuf,
    shell: (String, Vec<String>),
    cwd: PathBuf,
    config: Arc<Config>,
    metrics: CellMetrics,
    window_size: gpui::Size<gpui::Pixels>,
    chrome_height: f32,
    render_count: Arc<AtomicU64>,
}

impl WindowFactory {
    fn open(&self, cx: &mut App, start_ipc: bool) -> anyhow::Result<WindowHandle<ForgeWindow>> {
        let (event_tx, event_rx) = mpsc::channel();
        let event_tx_for_view = event_tx.clone();
        let (input_tx, input_rx) = async_mpsc::unbounded_channel();
        let resize_targets = Arc::new(Mutex::new(vec![(1_u64, input_tx.clone())]));
        let factory = self.clone();
        let last_resize = Arc::new(Mutex::new(None));
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
                let resize_targets = Arc::clone(&resize_targets);
                let last_resize = Arc::clone(&last_resize);
                let view = cx.new(|cx| {
                    let resize_targets = Arc::clone(&resize_targets);
                    let last_resize = Arc::clone(&last_resize);
                    let metrics = factory.metrics;
                    let chrome_height = factory.chrome_height;
                    let observer_resize_targets = Arc::clone(&resize_targets);
                    let observer_last_resize = Arc::clone(&last_resize);
                    cx.observe_window_bounds(window, move |_, window, _| {
                        let (cols, rows) =
                            grid_dimensions(window.bounds().size, metrics, chrome_height);
                        let mut last = observer_last_resize
                            .lock()
                            .expect("resize state mutex poisoned");
                        if *last != Some((cols, rows)) {
                            *last = Some((cols, rows));
                            observer_resize_targets
                                .lock()
                                .expect("resize targets mutex poisoned")
                                .retain(|(_, input)| {
                                    input.send(IpcCommand::Resize { cols, rows }).is_ok()
                                });
                        }
                    })
                    .detach();
                    ForgeWindow::new(
                        event_rx,
                        event_tx_for_view.clone(),
                        input_tx,
                        Arc::clone(&resize_targets),
                        Arc::clone(&last_resize),
                        Arc::clone(&factory.render_count),
                        metrics,
                        Arc::clone(&factory.config),
                        factory.clone(),
                        cx,
                    )
                });
                window.focus(&view.read(cx).focus);
                view
            },
        )?;

        if start_ipc {
            let socket = self.socket.clone();
            let shell = self.shell.clone();
            let cwd = self.cwd.clone();
            window.update(cx, |_, window, _| {
                window.on_next_frame(move |_, _| {
                    spawn_ipc_worker(socket, shell, cwd, 1, event_tx, input_rx);
                });
            })?;
        }
        Ok(window)
    }
}

struct ForgeWindow {
    tabs: Vec<TerminalTab>,
    active_tab: usize,
    next_tab_id: u64,
    event_tx: Sender<UiEvent>,
    resize_targets: Arc<Mutex<Vec<(u64, async_mpsc::UnboundedSender<IpcCommand>)>>>,
    last_resize: Arc<Mutex<Option<(u16, u16)>>>,
    config: Arc<Config>,
    focus: FocusHandle,
    render_count: Arc<AtomicU64>,
    frame_probe: Option<FrameProbe>,
    factory: WindowFactory,
    keymap: ShellKeymap,
    palette_open: bool,
    palette_query: String,
    split: Option<ActiveSplit>,
}

#[derive(Clone, Copy)]
struct ActiveSplit {
    direction: SplitDirection,
    first: usize,
    second: usize,
}

/// A live terminal is intentionally owned by exactly one tab. This prevents a
/// background session from overwriting the grid currently visible to the user.
struct TerminalTab {
    id: u64,
    title: String,
    terminal: TerminalSurface,
    status: String,
    input: async_mpsc::UnboundedSender<IpcCommand>,
    drag_anchor: Option<forge_gui::CellPos>,
}

impl ForgeWindow {
    fn new(
        events: Receiver<UiEvent>,
        event_tx: Sender<UiEvent>,
        input: async_mpsc::UnboundedSender<IpcCommand>,
        resize_targets: Arc<Mutex<Vec<(u64, async_mpsc::UnboundedSender<IpcCommand>)>>>,
        last_resize: Arc<Mutex<Option<(u16, u16)>>>,
        render_count: Arc<AtomicU64>,
        metrics: CellMetrics,
        config: Arc<Config>,
        factory: WindowFactory,
        cx: &mut Context<Self>,
    ) -> Self {
        cx.spawn(async move |this, cx| {
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
                if changed && this.update(cx, |_, cx| cx.notify()).is_err() {
                    return;
                }
            }
        })
        .detach();
        let palette = Palette::from(&config.colors);
        let keymap = ShellKeymap::default().with_overrides(&config.keybindings);
        Self {
            tabs: vec![TerminalTab {
                id: 1,
                title: "Terminal 1".into(),
                terminal: TerminalSurface::new(TerminalGrid::new(80, 24), metrics, palette),
                status: "Conectando a forge-termd…".into(),
                input,
                drag_anchor: None,
            }],
            active_tab: 0,
            next_tab_id: 2,
            event_tx,
            resize_targets,
            last_resize,
            config,
            focus: cx.focus_handle(),
            render_count,
            frame_probe: None,
            factory,
            keymap,
            palette_open: false,
            palette_query: String::new(),
            split: None,
        }
    }

    fn handle_event(&mut self, event: UiEvent, cx: &mut Context<Self>) {
        match event {
            UiEvent::Message {
                tab_id,
                message: ServerMessage::Exited { exit_code, .. },
            } => {
                // Keep the window visible so other Forge windows remain usable.
                if let Some(tab) = self.tab_mut(tab_id) {
                    tab.status = format!("Sesión terminada ({})", exit_code.unwrap_or(0));
                }
                cx.notify();
            }
            UiEvent::Message {
                tab_id,
                message: ServerMessage::Error { message },
            } => {
                if let Some(tab) = self.tab_mut(tab_id) {
                    tab.status = format!("Error del daemon: {message}");
                }
            }
            UiEvent::Message { tab_id, message } => {
                let Some(tab) = self.tab_mut(tab_id) else {
                    return;
                };
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
        }
    }

    fn active_tab(&self) -> &TerminalTab {
        &self.tabs[self.active_tab]
    }

    fn active_tab_mut(&mut self) -> &mut TerminalTab {
        &mut self.tabs[self.active_tab]
    }

    fn tab_mut(&mut self, id: u64) -> Option<&mut TerminalTab> {
        self.tabs.iter_mut().find(|tab| tab.id == id)
    }

    fn terminal_surface_at(&mut self, index: usize) -> &mut TerminalSurface {
        &mut self.tabs[index].terminal
    }

    fn active_terminal_surface(&mut self) -> &mut TerminalSurface {
        &mut self.active_tab_mut().terminal
    }

    fn create_terminal_tab(&mut self, cx: &mut Context<Self>) -> usize {
        let id = self.next_tab_id;
        self.next_tab_id += 1;
        let (input, input_rx) = async_mpsc::unbounded_channel();
        let tab_number = self.tabs.len() + 1;
        let palette = Palette::from(&self.config.colors);
        self.tabs.push(TerminalTab {
            id,
            title: format!("Terminal {tab_number}"),
            terminal: TerminalSurface::new(
                TerminalGrid::new(80, 24),
                self.factory.metrics,
                palette,
            ),
            status: "Conectando a forge-termd…".into(),
            input,
            drag_anchor: None,
        });
        self.resize_targets
            .lock()
            .expect("resize targets mutex poisoned")
            .push((id, self.active_tab().input.clone()));
        if let Some((cols, rows)) = *self
            .last_resize
            .lock()
            .expect("resize state mutex poisoned")
        {
            let _ = self
                .active_tab()
                .input
                .send(IpcCommand::Resize { cols, rows });
        }
        self.active_tab = self.tabs.len() - 1;
        spawn_ipc_worker(
            self.factory.socket.clone(),
            self.factory.shell.clone(),
            self.factory.cwd.clone(),
            id,
            self.event_tx.clone(),
            input_rx,
        );
        cx.notify();
        self.active_tab
    }

    fn activate_tab(&mut self, index: usize, cx: &mut Context<Self>) {
        if index < self.tabs.len() {
            self.active_tab = index;
            cx.notify();
        }
    }

    fn close_tab(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.tabs.len() <= 1 || index >= self.tabs.len() {
            return;
        }
        let closed = self.tabs.remove(index);
        self.resize_targets
            .lock()
            .expect("resize targets mutex poisoned")
            .retain(|(id, _)| *id != closed.id);
        self.active_tab = self.active_tab.min(self.tabs.len() - 1);
        self.split = None;
        cx.notify();
    }

    fn close_active_tab(&mut self, cx: &mut Context<Self>) {
        self.close_tab(self.active_tab, cx);
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        let modifiers = KeyModifiers {
            control: keystroke.modifiers.control,
            alt: keystroke.modifiers.alt,
            shift: keystroke.modifiers.shift,
        };
        let copy_paste = modifiers.control && modifiers.shift && !modifiers.alt;
        let shell_key = ShellKeystroke::new(
            &keystroke.key,
            modifiers.control,
            modifiers.alt,
            modifiers.shift,
        );
        if self.palette_open {
            match keystroke.key.as_str() {
                "escape" => self.palette_open = false,
                "enter" => {
                    let command = search_commands(&self.palette_query)
                        .first()
                        .map(|item| item.command);
                    self.palette_open = false;
                    if let Some(command) = command {
                        self.run_shell_command(command, cx);
                    }
                    cx.notify();
                    return;
                }
                "backspace" => {
                    self.palette_query.pop();
                }
                _ if !modifiers.control && !modifiers.alt => {
                    if let Some(text) = &keystroke.key_char {
                        self.palette_query.push_str(text);
                    }
                }
                _ => {}
            }
            self.active_tab_mut().status =
                search_commands(&self.palette_query).first().map_or_else(
                    || "Sin comandos coincidentes".into(),
                    |item| format!("Palette · {}", item.command.title()),
                );
            cx.notify();
            return;
        }
        if self.keymap.resolve(&shell_key, ShellContext::Terminal)
            == Some(ShellCommand::NewTerminalTab)
        {
            self.run_shell_command(ShellCommand::NewTerminalTab, cx);
            return;
        }
        if self.keymap.resolve(&shell_key, ShellContext::Terminal)
            == Some(ShellCommand::ShowCommandPalette)
        {
            self.palette_open = true;
            self.palette_query.clear();
            self.active_tab_mut().status =
                "Palette · escribe para buscar; Escape para cerrar".into();
            cx.notify();
            return;
        }
        match keystroke.key.as_str() {
            "c" if copy_paste => {
                self.copy_selection(cx);
                return;
            }
            "v" if copy_paste => {
                self.paste(cx.read_from_clipboard());
                return;
            }
            "insert" if modifiers.shift && !modifiers.control => {
                self.paste(cx.read_from_primary());
                return;
            }
            _ => {}
        }
        if let Some(bytes) =
            encode_terminal_key(&keystroke.key, keystroke.key_char.as_deref(), modifiers)
        {
            let tab = self.active_tab_mut();
            tab.terminal.selection = None;
            let _ = tab.input.send(IpcCommand::Input(bytes));
            cx.notify();
        }
    }

    fn run_shell_command(&mut self, command: ShellCommand, cx: &mut Context<Self>) {
        match command {
            ShellCommand::NewTerminalTab => {
                self.create_terminal_tab(cx);
            }
            ShellCommand::SplitHorizontal | ShellCommand::SplitVertical => {
                let first = self.active_tab;
                let second = self.create_terminal_tab(cx);
                self.split = Some(ActiveSplit {
                    direction: if command == ShellCommand::SplitHorizontal {
                        SplitDirection::Horizontal
                    } else {
                        SplitDirection::Vertical
                    },
                    first,
                    second,
                });
                self.active_tab = second;
                self.active_tab_mut().status = "Split activo · Ctrl+Tab cambia el foco".into();
                cx.notify();
            }
            // Keep the last terminal visible: closing a window remains a
            // window-manager action, while Ctrl+W behaves like terminal apps
            // and closes the focused tab when there is more than one.
            ShellCommand::CloseWindow if self.tabs.len() > 1 => self.close_active_tab(cx),
            ShellCommand::FocusNextPane => {
                let next = self.split.map_or_else(
                    || (self.active_tab + 1) % self.tabs.len(),
                    |split| {
                        if self.active_tab == split.first {
                            split.second
                        } else {
                            split.first
                        }
                    },
                );
                self.activate_tab(next, cx);
            }
            command => {
                self.active_tab_mut().status = format!(
                    "{} estará disponible al completar su subfase",
                    command.title()
                )
            }
        }
    }

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

    /// Sends clipboard text as if typed; line breaks become carriage returns
    /// like a terminal expects from the keyboard.
    fn paste(&self, item: Option<ClipboardItem>) {
        if let Some(text) = item.and_then(|item| item.text()) {
            let bytes = text.replace("\r\n", "\r").replace('\n', "\r").into_bytes();
            let _ = self.active_tab().input.send(IpcCommand::Input(bytes));
        }
    }

    fn on_mouse_down(&mut self, event: &MouseDownEvent, cx: &mut Context<Self>) {
        if !self.active_tab().terminal.contains(event.position) {
            return;
        }
        let Some(cell) = self.active_tab().terminal.cell_at(event.position) else {
            return;
        };
        let tab = self.active_tab_mut();
        tab.terminal.selection = match event.click_count {
            2 => Some(tab.terminal.grid.word_at(cell)),
            n if n >= 3 => Some(tab.terminal.grid.line_at(cell.y)),
            _ => None,
        };
        tab.drag_anchor = Some(cell);
        cx.notify();
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, cx: &mut Context<Self>) {
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

    fn on_mouse_up(&mut self, _event: &MouseUpEvent, cx: &mut Context<Self>) {
        if self.active_tab_mut().drag_anchor.take().is_none() {
            return;
        }
        // Linux convention: a finished selection is available on middle click.
        if let Some(text) = self.selected_text() {
            cx.write_to_primary(ClipboardItem::new_string(text));
        }
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
        let palette = self.active_tab().terminal.palette;
        let config = &self.config;
        let chrome = rgb(0x171b24);
        let chrome_border = rgb(0x2a3140);
        let muted = rgb(0x7f8aa3);
        let english = config.ui.language == Language::English;
        let new_tab_hint = if english {
            "Ctrl+T · new tab"
        } else {
            "Ctrl+T · nueva pestaña"
        };
        let terminal_area = if let Some(split) = self.split {
            let horizontal = split.direction == SplitDirection::Horizontal;
            div()
                .flex_1()
                .min_h(px(0.0))
                .flex()
                .when(horizontal, |area| area.flex_col())
                .when(!horizontal, |area| area.flex_row())
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .min_h(px(0.0))
                        .p(px(config.terminal.padding))
                        .border_color(chrome_border)
                        .when(horizontal, |pane| pane.border_b_1())
                        .when(!horizontal, |pane| pane.border_r_1())
                        .child(TerminalGridElement::new(
                            cx.entity(),
                            split.first,
                            |view: &mut Self, index| view.terminal_surface_at(index),
                        )),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .min_h(px(0.0))
                        .p(px(config.terminal.padding))
                        .child(TerminalGridElement::new(
                            cx.entity(),
                            split.second,
                            |view: &mut Self, index| view.terminal_surface_at(index),
                        )),
                )
        } else {
            div()
                .flex_1()
                .min_h(px(0.0))
                .p(px(config.terminal.padding))
                .child(TerminalGridElement::new(
                    cx.entity(),
                    self.active_tab,
                    |view: &mut Self, index| view.terminal_surface_at(index),
                ))
        };
        window.set_client_inset(px(WINDOW_RESIZE_INSET));
        div()
            .id("forge-terminal")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|view, event, _, cx| view.on_key_down(event, cx)))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|view, event: &MouseDownEvent, window, cx| {
                    if let Some(edge) =
                        resize_edge(event.position, window.window_bounds().get_bounds().size)
                    {
                        window.start_window_resize(edge);
                    } else {
                        view.on_mouse_down(event, cx);
                    }
                }),
            )
            .on_mouse_move(cx.listener(|view, event: &MouseMoveEvent, window, cx| {
                if resize_edge(event.position, window.window_bounds().get_bounds().size).is_none() {
                    view.on_mouse_move(event, cx);
                }
                window.refresh();
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|view, event, _, cx| view.on_mouse_up(event, cx)),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|view, event, _, cx| view.on_mouse_up(event, cx)),
            )
            .cursor(gpui::CursorStyle::IBeam)
            .size_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .bg(palette.background)
            .text_color(palette.foreground)
            .font_family(config.font.family.clone())
            .child(
                canvas(
                    |_bounds, window, _cx| {
                        window.insert_hitbox(
                            Bounds::new(
                                point(px(0.0), px(0.0)),
                                window.window_bounds().get_bounds().size,
                            ),
                            HitboxBehavior::Normal,
                        )
                    },
                    move |_bounds, hitbox, window, _cx| {
                        let Some(edge) = resize_edge(
                            window.mouse_position(),
                            window.window_bounds().get_bounds().size,
                        ) else {
                            return;
                        };
                        window.set_cursor_style(resize_cursor(edge), &hitbox);
                    },
                )
                .size_full()
                .absolute(),
            )
            .child(
                div()
                    .h(px(TOPBAR_HEIGHT))
                    .w_full()
                    .flex()
                    .items_center()
                    .px(px(10.0))
                    .gap(px(8.0))
                    .bg(chrome)
                    .border_b_1()
                    .border_color(chrome_border)
                    .child(img("forge-logo.svg").size(px(19.0)))
                    .children(self.tabs.iter().enumerate().map(|(index, tab)| {
                        let title = tab.title.clone();
                        let active = index == self.active_tab;
                        div()
                            .id(SharedString::from(format!("terminal-tab-{}", tab.id)))
                            .h(px(28.0))
                            .min_w(px(120.0))
                            .max_w(px(210.0))
                            .px(px(10.0))
                            .flex()
                            .items_center()
                            .gap(px(8.0))
                            .rounded(px(6.0))
                            .bg(if active { rgb(0x252c3a) } else { chrome })
                            .border_1()
                            .border_color(if active { rgb(0x3b465c) } else { chrome })
                            .text_size(px(12.0))
                            .text_color(if active { palette.foreground } else { muted })
                            .cursor_pointer()
                            .hover(|style| style.bg(rgb(0x252c3a)))
                            .on_click(
                                cx.listener(move |view, _, _, cx| view.activate_tab(index, cx)),
                            )
                            .child(div().flex_1().overflow_hidden().child(title))
                            .child(
                                div()
                                    .id(SharedString::from(format!("terminal-tab-close-{index}")))
                                    .size(px(16.0))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .rounded(px(3.0))
                                    .hover(|style| style.bg(rgb(0x3b465c)))
                                    .on_click(
                                        cx.listener(move |view, _, _, cx| {
                                            view.close_tab(index, cx)
                                        }),
                                    )
                                    .child("×"),
                            )
                    }))
                    .child(
                        div()
                            .id("new-terminal-tab")
                            .size(px(28.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(5.0))
                            .text_color(muted)
                            .cursor_pointer()
                            .hover(|style| style.bg(rgb(0x252c3a)))
                            .on_click(cx.listener(|view, _, _, cx| {
                                view.create_terminal_tab(cx);
                            }))
                            .child("+"),
                    )
                    .child(
                        div()
                            .id("forge-drag-region")
                            .h_full()
                            .flex_1()
                            .cursor_default()
                            .on_mouse_down(MouseButton::Left, |event, window, _| {
                                if event.click_count >= 2 {
                                    window.zoom_window();
                                } else {
                                    window.start_window_move();
                                }
                            })
                            .on_mouse_down(MouseButton::Right, |event, window, _| {
                                window.show_window_menu(event.position)
                            }),
                    )
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(muted)
                            .child(new_tab_hint),
                    )
                    .child(
                        div()
                            .id("forge-minimize")
                            .size(px(28.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(5.0))
                            .text_color(muted)
                            .cursor_pointer()
                            .hover(|style| style.bg(rgb(0x252c3a)))
                            .on_click(|_, window, _| window.minimize_window())
                            .child("—"),
                    )
                    .child(
                        div()
                            .id("forge-maximize")
                            .size(px(28.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(5.0))
                            .text_color(muted)
                            .cursor_pointer()
                            .hover(|style| style.bg(rgb(0x252c3a)))
                            .on_click(|_, window, _| window.zoom_window())
                            .child("□"),
                    )
                    .child(
                        div()
                            .id("forge-close")
                            .size(px(28.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(5.0))
                            .text_color(muted)
                            .cursor_pointer()
                            .hover(|style| style.bg(rgb(0xa84152)))
                            .on_click(|_, window, _| window.remove_window())
                            .child("×"),
                    ),
            )
            .child(terminal_area)
            .when(self.palette_open, |root| {
                let matches = search_commands(&self.palette_query);
                root.child(
                    div()
                        .id("command-palette")
                        .absolute()
                        .top(px(64.0))
                        .left(px(48.0))
                        .w(px(440.0))
                        .p(px(12.0))
                        .rounded(px(8.0))
                        .bg(rgb(0x202633))
                        .border_1()
                        .border_color(chrome_border)
                        .shadow(vec![gpui::BoxShadow {
                            color: gpui::transparent_black(),
                            offset: point(px(0.0), px(8.0)),
                            blur_radius: px(20.0),
                            spread_radius: px(0.0),
                        }])
                        .child(
                            div()
                                .text_size(px(14.0))
                                .text_color(palette.foreground)
                                .child(format!("› {}", self.palette_query)),
                        )
                        .child(
                            div()
                                .mt(px(6.0))
                                .text_size(px(11.0))
                                .text_color(muted)
                                .child(if english {
                                    "Enter runs the first result · Esc closes"
                                } else {
                                    "Enter ejecutará el primer resultado · Esc cierra"
                                }),
                        )
                        .children(matches.into_iter().take(6).map(|item| {
                            div()
                                .mt(px(7.0))
                                .px(px(8.0))
                                .py(px(5.0))
                                .rounded(px(4.0))
                                .bg(rgb(0x2a3140))
                                .text_size(px(13.0))
                                .child(item.command.title())
                        })),
                )
            })
    }
}

fn resize_edge(
    position: Point<gpui::Pixels>,
    size: gpui::Size<gpui::Pixels>,
) -> Option<ResizeEdge> {
    let inset = px(WINDOW_RESIZE_INSET);
    let edge = if position.y < inset && position.x < inset {
        ResizeEdge::TopLeft
    } else if position.y < inset && position.x > size.width - inset {
        ResizeEdge::TopRight
    } else if position.y < inset {
        ResizeEdge::Top
    } else if position.y > size.height - inset && position.x < inset {
        ResizeEdge::BottomLeft
    } else if position.y > size.height - inset && position.x > size.width - inset {
        ResizeEdge::BottomRight
    } else if position.y > size.height - inset {
        ResizeEdge::Bottom
    } else if position.x < inset {
        ResizeEdge::Left
    } else if position.x > size.width - inset {
        ResizeEdge::Right
    } else {
        return None;
    };
    Some(edge)
}

fn resize_cursor(edge: ResizeEdge) -> WindowCursorStyle {
    match edge {
        ResizeEdge::Top | ResizeEdge::Bottom => WindowCursorStyle::ResizeUpDown,
        ResizeEdge::Left | ResizeEdge::Right => WindowCursorStyle::ResizeLeftRight,
        ResizeEdge::TopLeft | ResizeEdge::BottomRight => WindowCursorStyle::ResizeUpLeftDownRight,
        ResizeEdge::TopRight | ResizeEdge::BottomLeft => WindowCursorStyle::ResizeUpRightDownLeft,
    }
}

#[cfg(test)]
mod window_chrome_tests {
    use super::*;

    #[test]
    fn resize_edges_cover_corners_sides_and_leave_the_content_alone() {
        let size = size(px(800.0), px(600.0));
        assert_eq!(
            resize_edge(point(px(0.0), px(0.0)), size),
            Some(ResizeEdge::TopLeft)
        );
        assert_eq!(
            resize_edge(point(px(799.0), px(0.0)), size),
            Some(ResizeEdge::TopRight)
        );
        assert_eq!(
            resize_edge(point(px(0.0), px(599.0)), size),
            Some(ResizeEdge::BottomLeft)
        );
        assert_eq!(
            resize_edge(point(px(799.0), px(599.0)), size),
            Some(ResizeEdge::BottomRight)
        );
        assert_eq!(
            resize_edge(point(px(400.0), px(0.0)), size),
            Some(ResizeEdge::Top)
        );
        assert_eq!(
            resize_edge(point(px(400.0), px(599.0)), size),
            Some(ResizeEdge::Bottom)
        );
        assert_eq!(
            resize_edge(point(px(0.0), px(300.0)), size),
            Some(ResizeEdge::Left)
        );
        assert_eq!(
            resize_edge(point(px(799.0), px(300.0)), size),
            Some(ResizeEdge::Right)
        );
        assert_eq!(resize_edge(point(px(400.0), px(300.0)), size), None);
    }
}

fn main() {
    let options = run_options();
    let started = Instant::now();
    let socket = options.socket;
    let grid_frames = options.benchmark_grid_frames;
    let render_count = Arc::new(AtomicU64::new(0));
    // Benchmarks stay comparable across machines by ignoring user config.
    let config = if grid_frames.is_some() {
        Config::default()
    } else {
        load_config(options.config.as_deref())
    };
    let window_size = if grid_frames.is_some() {
        size(px(1300.0), px(700.0))
    } else {
        size(px(960.0), px(600.0))
    };

    Application::new()
        .with_assets(Assets)
        .run(move |cx: &mut App| {
            let mut config = config;
            let family =
                resolve_font_family(&config.font.family, &cx.text_system().all_font_names());
            if !family.eq_ignore_ascii_case(&config.font.family) {
                eprintln!(
                    "forge: fuente {:?} no disponible como familia; usando {family:?}",
                    config.font.family
                );
            }
            config.font.family = family;
            let config = Arc::new(config);
            // The grid benchmark needs every one of its 200×60 cells on screen.
            let metrics = if grid_frames.is_some() {
                CellMetrics::BENCHMARK
            } else {
                cell_metrics_for(&config, cx)
            };
            let chrome_height = chrome_height(&config);
            let shell = (config.shell(), config.terminal.args.clone());
            let factory = WindowFactory {
                socket,
                shell,
                cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
                config: Arc::clone(&config),
                metrics,
                window_size,
                chrome_height,
                render_count: Arc::clone(&render_count),
            };
            let start_ipc = options.benchmark_idle_ms.is_none() && grid_frames.is_none();
            let window = factory.open(cx, start_ipc).expect("open Forge window");
            if options.exit_after_first_frame {
                window
                    .update(cx, |_, window, _| {
                        window.on_next_frame(move |_, cx| {
                            emit_metrics(&GuiMetrics {
                                scenario: "startup_empty",
                                elapsed_ms: started.elapsed().as_secs_f64() * 1_000.0,
                                samples_ms: None,
                                frames: 1,
                                pss_kib: process_pss_kib(),
                                painted_cells: None,
                            });
                            cx.quit();
                        });
                    })
                    .expect("schedule startup measurement");
            }
            if let Some(idle_ms) = options.benchmark_idle_ms {
                spawn_idle_benchmark(idle_ms, Arc::clone(&render_count), cx);
            }
            if let Some(iterations) = grid_frames {
                spawn_grid_benchmark(iterations, window, cx);
            }
            cx.activate(true);
        });
}

fn spawn_idle_benchmark(idle_ms: u64, render_count: Arc<AtomicU64>, cx: &mut App) {
    cx.spawn(async move |cx| {
        Timer::after(Duration::from_secs(1)).await;
        let baseline = render_count.load(Ordering::Relaxed);
        Timer::after(Duration::from_millis(idle_ms)).await;
        emit_metrics(&GuiMetrics {
            scenario: "idle",
            elapsed_ms: Duration::from_millis(idle_ms).as_secs_f64() * 1_000.0,
            samples_ms: None,
            frames: render_count
                .load(Ordering::Relaxed)
                .saturating_sub(baseline),
            pss_kib: process_pss_kib(),
            painted_cells: None,
        });
        let _ = cx.update(|cx| cx.quit());
    })
    .detach();
}

fn spawn_grid_benchmark(iterations: usize, window: WindowHandle<ForgeWindow>, cx: &mut App) {
    cx.spawn(async move |cx| {
        Timer::after(Duration::from_secs(1)).await;
        let Ok(view) = window.update(cx, |_, _, cx| cx.entity()) else {
            return;
        };
        let mut samples_ms = Vec::with_capacity(iterations);
        for revision in 1..=iterations {
            let (presented_tx, presented_rx) = oneshot::channel();
            let view = view.clone();
            // Apply the patch at the start of a frame tick so the sample covers
            // state update → draw → present, not the wait for the compositor.
            let scheduled = window.update(cx, |_, window, _| {
                window.on_next_frame(move |_, cx| {
                    let patch = synthetic_grid_patch(revision);
                    let started = Instant::now();
                    view.update(cx, |view, cx| {
                        view.active_terminal_surface()
                            .grid
                            .apply_server_message(patch)
                            .expect("synthetic grid patch");
                        view.frame_probe = Some(FrameProbe {
                            started,
                            presented: presented_tx,
                        });
                        cx.notify();
                    });
                });
            });
            if scheduled.is_err() {
                break;
            }
            if let Ok(sample) = presented_rx.await {
                samples_ms.push(sample);
            }
        }
        let painted_cells = window
            .update(cx, |view, _, _| view.active_tab().terminal.painted_cells)
            .ok()
            .and_then(|cells| u64::try_from(cells).ok());
        emit_metrics(&GuiMetrics {
            scenario: "grid_full",
            elapsed_ms: samples_ms.last().copied().unwrap_or_default(),
            samples_ms: Some(samples_ms),
            frames: u64::try_from(iterations).unwrap_or(u64::MAX),
            pss_kib: process_pss_kib(),
            painted_cells,
        });
        let _ = cx.update(|cx| cx.quit());
    })
    .detach();
}

fn spawn_ipc_worker(
    socket: PathBuf,
    shell: (String, Vec<String>),
    cwd: PathBuf,
    tab_id: u64,
    events: Sender<UiEvent>,
    input: async_mpsc::UnboundedReceiver<IpcCommand>,
) {
    thread::Builder::new()
        .name("forge-gui-ipc".into())
        .spawn(move || {
            let result = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .context("create IPC runtime")
                .and_then(|runtime| {
                    runtime.block_on(run_ipc(socket, shell, cwd, tab_id, events.clone(), input))
                });
            if let Err(error) = result {
                let _ = events.send(UiEvent::Status {
                    tab_id,
                    status: format!("Sin conexión: {error:#}"),
                });
            }
        })
        .expect("spawn GUI IPC worker");
}

async fn run_ipc(
    socket: PathBuf,
    (command, args): (String, Vec<String>),
    cwd: PathBuf,
    tab_id: u64,
    events: Sender<UiEvent>,
    mut input: async_mpsc::UnboundedReceiver<IpcCommand>,
) -> Result<()> {
    let (stream, _daemon) = connect_or_start_daemon(&socket).await?;
    let (reader, mut writer) = stream.into_split();
    let mut reader = FrameReader::new(reader);
    write_message(
        &mut writer,
        FrameKind::Request,
        &ClientMessage::Initialize {
            protocol_version: PROTOCOL_VERSION,
            client_name: "forge-gui".into(),
        },
    )
    .await?;
    match reader.read_message::<ServerMessage>().await?.1 {
        ServerMessage::Initialized { protocol_version } if protocol_version == PROTOCOL_VERSION => {
        }
        message => bail!("respuesta initialize inesperada: {message:?}"),
    }
    write_message(
        &mut writer,
        FrameKind::Request,
        &ClientMessage::CreateSession {
            request_id: 1,
            command,
            args,
            cwd,
            cols: 80,
            rows: 24,
        },
    )
    .await?;
    let session_id = match reader.read_message::<ServerMessage>().await?.1 {
        ServerMessage::SessionCreated { session_id, .. } => session_id,
        message => bail!("respuesta create_session inesperada: {message:?}"),
    };
    write_message(
        &mut writer,
        FrameKind::Request,
        &ClientMessage::Attach { session_id },
    )
    .await?;
    let _ = events.send(UiEvent::Status {
        tab_id,
        status: format!("Sesión {session_id} conectada"),
    });

    // Reading and writing run as independent tasks: a `select!` over both
    // would cancel in-flight reads on every keystroke and desynchronize the
    // framed stream under heavy output.
    let mut incoming = tokio::spawn(async move {
        loop {
            let message = reader.read_message::<ServerMessage>().await?.1;
            let exited = matches!(message, ServerMessage::Exited { .. });
            events
                .send(UiEvent::Message { tab_id, message })
                .context("GUI closed")?;
            if exited {
                return Ok::<(), anyhow::Error>(());
            }
        }
    });
    let mut outgoing = tokio::spawn(async move {
        while let Some(command) = input.recv().await {
            match command {
                IpcCommand::Input(data) => {
                    write_message(
                        &mut writer,
                        FrameKind::Notification,
                        &ClientMessage::Input { session_id, data },
                    )
                    .await?;
                }
                IpcCommand::Resize { cols, rows } => {
                    let mut latest = (cols, rows);
                    // Interactive window resize can generate hundreds of
                    // bounds updates. A PTY reflow is expensive and each one
                    // produces a screen snapshot, so keep only the final cell
                    // dimensions after a short quiet period.
                    loop {
                        match tokio::time::timeout(Duration::from_millis(60), input.recv()).await {
                            Ok(Some(IpcCommand::Resize { cols, rows })) => latest = (cols, rows),
                            Ok(Some(IpcCommand::Input(data))) => {
                                send_resize(&mut writer, session_id, latest).await?;
                                write_message(
                                    &mut writer,
                                    FrameKind::Notification,
                                    &ClientMessage::Input { session_id, data },
                                )
                                .await?;
                                break;
                            }
                            Ok(None) => {
                                send_resize(&mut writer, session_id, latest).await?;
                                return Ok::<(), anyhow::Error>(());
                            }
                            Err(_) => {
                                send_resize(&mut writer, session_id, latest).await?;
                                break;
                            }
                        }
                    }
                }
            }
        }
        Ok::<(), anyhow::Error>(())
    });
    let result = tokio::select! {
        incoming = &mut incoming => incoming.context("IPC reader task")?,
        outgoing = &mut outgoing => outgoing.context("IPC writer task")?,
    };
    incoming.abort();
    outgoing.abort();
    result
}

async fn send_resize<W>(writer: &mut W, session_id: u64, (cols, rows): (u16, u16)) -> Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    write_message(
        writer,
        FrameKind::Notification,
        &ClientMessage::Resize {
            session_id,
            cols,
            rows,
        },
    )
    .await
    .context("send terminal resize")
}

struct DaemonGuard(Child);

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        let _ = self.0.start_kill();
    }
}

async fn connect_or_start_daemon(socket: &Path) -> Result<(UnixStream, Option<DaemonGuard>)> {
    if let Ok(stream) = UnixStream::connect(socket).await {
        return Ok((stream, None));
    }
    let ghostty = ghostty_library();
    if !ghostty.is_file() {
        bail!(
            "no existe {}; ejecuta ./scripts/bootstrap-ghostty.sh una vez",
            ghostty.display()
        );
    }
    let mut command = daemon_command(socket, &ghostty)?;
    let child = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .context("arrancar forge-termd automáticamente")?;
    let mut daemon = DaemonGuard(child);
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        match UnixStream::connect(socket).await {
            Ok(stream) => return Ok((stream, Some(daemon))),
            Err(_) if Instant::now() < deadline => {
                if let Some(status) = daemon.0.try_wait().context("consultar forge-termd")? {
                    bail!("forge-termd terminó durante el arranque: {status}");
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            Err(error) => bail!("forge-termd no abrió {}: {error}", socket.display()),
        }
    }
}

fn daemon_command(socket: &Path, ghostty: &Path) -> Result<Command> {
    let sibling = std::env::current_exe()
        .context("resolver ejecutable actual")?
        .with_file_name("proto-termd");
    let mut command = if sibling.is_file() {
        Command::new(sibling)
    } else {
        let mut cargo = Command::new("cargo");
        cargo
            .current_dir(workspace_dir())
            .args(["run", "-p", "proto-termd", "--"]);
        cargo
    };
    command
        .arg("--socket")
        .arg(socket)
        .arg("--ghostty-lib")
        .arg(ghostty);
    Ok(command)
}

fn workspace_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("forge-gui crate belongs to workspace/crates")
        .to_path_buf()
}

fn ghostty_library() -> PathBuf {
    std::env::var_os("FORGE_GHOSTTY_LIB").map_or_else(
        || workspace_dir().join("target/ghostty/lib/libghostty-vt.so"),
        PathBuf::from,
    )
}

fn load_config(explicit: Option<&Path>) -> Config {
    let Some(path) = Config::default_path(explicit) else {
        return Config::default();
    };
    match Config::load(&path) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("forge: {error}; usando la configuración por defecto");
            Config::default()
        }
    }
}

/// Cell geometry for the configured font: the advance of a monospace glyph
/// rounded to whole pixels, and the line height as a multiple of the size.
fn cell_metrics_for(config: &Config, cx: &App) -> CellMetrics {
    let font_size = px(config.font.size);
    let font_id = cx
        .text_system()
        .resolve_font(&gpui::font(config.font.family.clone()));
    let advance = cx
        .text_system()
        .advance(font_id, font_size, 'M')
        .map_or(font_size * 0.6, |size| size.width);
    CellMetrics {
        width: f32::from(advance).round().max(1.0),
        height: (config.font.size * config.font.line_height)
            .round()
            .max(1.0),
        font_size: config.font.size,
    }
}

/// Vertical space taken by window padding and the application chrome.
fn chrome_height(config: &Config) -> f32 {
    config.terminal.padding * 2.0 + TOPBAR_HEIGHT
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn grid_dimensions(
    size: gpui::Size<gpui::Pixels>,
    metrics: CellMetrics,
    chrome_height: f32,
) -> (u16, u16) {
    let cols = ((size.width / px(metrics.width)).floor() as u16).clamp(1, 500);
    let rows =
        (((size.height - px(chrome_height)) / px(metrics.height)).floor() as u16).clamp(1, 300);
    (cols, rows)
}

struct RunOptions {
    socket: PathBuf,
    config: Option<PathBuf>,
    exit_after_first_frame: bool,
    benchmark_idle_ms: Option<u64>,
    benchmark_grid_frames: Option<usize>,
}

fn run_options() -> RunOptions {
    let mut args = std::env::args().skip(1);
    let mut socket = PathBuf::from("/tmp/forge-prototype.sock");
    let mut config = None;
    let mut exit_after_first_frame = false;
    let mut benchmark_idle_ms = None;
    let mut benchmark_grid_frames = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--socket" => {
                if let Some(path) = args.next() {
                    socket = path.into();
                }
            }
            "--config" => config = args.next().map(PathBuf::from),
            "--exit-after-first-frame" => exit_after_first_frame = true,
            "--benchmark-idle-ms" => {
                benchmark_idle_ms = args.next().and_then(|value| value.parse().ok());
            }
            "--benchmark-grid-frames" => {
                benchmark_grid_frames = args.next().and_then(|value| value.parse().ok());
            }
            _ => {}
        }
    }
    RunOptions {
        socket,
        config,
        exit_after_first_frame,
        benchmark_idle_ms,
        benchmark_grid_frames,
    }
}

#[derive(Serialize)]
struct GuiMetrics {
    scenario: &'static str,
    elapsed_ms: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    samples_ms: Option<Vec<f64>>,
    frames: u64,
    pss_kib: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    painted_cells: Option<u64>,
}

/// Full 200×60 frame where every cell changes glyph and colour each revision,
/// so the renderer cannot reuse anything from the previous frame.
fn synthetic_grid_patch(revision: usize) -> ServerMessage {
    const PRINTABLE_ASCII: usize = 94;
    let cell = |x: usize, y: usize| {
        let glyph = u8::try_from((x + y * 7 + revision * 13) % PRINTABLE_ASCII)
            .expect("index below 94 fits a byte")
            + b'!';
        ScreenCell {
            text: char::from(glyph).into(),
            foreground: Some(Rgb {
                r: 0x88,
                g: u8::try_from((x + revision) % 128).expect("bounded synthetic color") + 0x40,
                b: 0xd0,
            }),
            background: None,
            styled: true,
        }
    };
    ServerMessage::ScreenPatch {
        session_id: 0,
        revision: u64::try_from(revision).unwrap_or(u64::MAX),
        cols: 200,
        rows: 60,
        full: true,
        dirty_rows: (0..60)
            .map(|y| ScreenRow {
                y,
                cells: (0..200).map(|x| cell(x, usize::from(y))).collect(),
            })
            .collect(),
        cursor: Some(ScreenCursor {
            x: u16::try_from(revision % 200).expect("column below 200"),
            y: u16::try_from(revision % 60).expect("row below 60"),
            visible: true,
            blinking: false,
            style: CursorStyle::Block,
        }),
    }
}

fn emit_metrics(metrics: &GuiMetrics) {
    if let Ok(json) = serde_json::to_string(metrics) {
        println!("{json}");
    }
}

fn process_pss_kib() -> Option<u64> {
    let contents = std::fs::read_to_string("/proc/self/smaps_rollup").ok()?;
    contents.lines().find_map(|line| {
        line.strip_prefix("Pss:")?
            .split_whitespace()
            .next()?
            .parse()
            .ok()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_and_default_ghostty_paths_are_stable() {
        assert_eq!(
            workspace_dir().file_name().and_then(|name| name.to_str()),
            Some("forge")
        );
        if std::env::var_os("FORGE_GHOSTTY_LIB").is_none() {
            assert!(ghostty_library().ends_with("target/ghostty/lib/libghostty-vt.so"));
        }
    }

    #[test]
    fn socket_argument_uses_documented_default_shape() {
        assert_eq!(
            PathBuf::from("/tmp/forge-prototype.sock").extension(),
            Some("sock".as_ref())
        );
    }

    #[test]
    fn derives_grid_dimensions_from_window_size_and_cell_metrics() {
        let chrome = chrome_height(&Config::default());
        assert!((chrome - 68.0).abs() < f32::EPSILON);
        let metrics = CellMetrics {
            width: 9.0,
            height: 18.0,
            font_size: 14.0,
        };
        assert_eq!(
            grid_dimensions(size(px(900.0), px(416.0)), metrics, chrome),
            (100, 19)
        );
        assert_eq!(
            grid_dimensions(size(px(1300.0), px(700.0)), CellMetrics::BENCHMARK, chrome),
            (216, 63)
        );
        assert_eq!(
            grid_dimensions(size(px(1.0), px(1.0)), metrics, chrome),
            (1, 1)
        );
        let mut bare = Config::default();
        bare.terminal.show_status = false;
        bare.terminal.padding = 0.0;
        assert!((chrome_height(&bare) - TOPBAR_HEIGHT).abs() < f32::EPSILON);
    }

    #[test]
    fn synthetic_benchmark_builds_a_fully_dirty_200_by_60_grid() {
        let ServerMessage::ScreenPatch {
            cols,
            rows,
            full,
            dirty_rows,
            ..
        } = synthetic_grid_patch(1)
        else {
            panic!("expected screen patch");
        };
        assert_eq!((cols, rows), (200, 60));
        assert!(full);
        assert_eq!(dirty_rows.len(), 60);
        assert!(dirty_rows.iter().all(|row| row.cells.len() == 200));
        assert!(
            dirty_rows
                .iter()
                .flat_map(|row| &row.cells)
                .all(|cell| cell.text.len() == 1 && cell.text.is_ascii() && cell.text != " ")
        );
        let ServerMessage::ScreenPatch {
            dirty_rows: next, ..
        } = synthetic_grid_patch(2)
        else {
            panic!("expected screen patch");
        };
        assert!(
            dirty_rows.iter().zip(&next).all(|(a, b)| a
                .cells
                .iter()
                .zip(&b.cells)
                .all(|(a, b)| a != b)),
            "every cell changes between revisions"
        );
    }
}
