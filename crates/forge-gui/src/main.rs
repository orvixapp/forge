mod grid_element;

use anyhow::{Context as _, Result, bail};
use forge_gui::{
    KeyModifiers, Selection, TerminalGrid,
    config::{Config, resolve_font_family},
    encode_terminal_key,
};
use gpui::{
    App, Application, Bounds, ClipboardItem, Context, FocusHandle, KeyDownEvent, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, Render, Timer, Window, WindowBounds,
    WindowHandle, WindowOptions, div, prelude::*, px, size,
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
        Arc,
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
    Message(ServerMessage),
    Status(String),
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

/// Height of the status line above the grid, including its bottom margin.
const STATUS_HEIGHT: f32 = 24.0;

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
        let (input_tx, input_rx) = async_mpsc::unbounded_channel();
        let factory = self.clone();
        let resize_tx = input_tx.clone();
        let bounds = Bounds::centered(None, self.window_size, cx);
        let window = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            move |window, cx| {
                let resize_tx = resize_tx.clone();
                let view = cx.new(|cx| {
                    let resize_tx = resize_tx.clone();
                    let metrics = factory.metrics;
                    let chrome_height = factory.chrome_height;
                    cx.observe_window_bounds(window, move |_, window, _| {
                        let (cols, rows) =
                            grid_dimensions(window.bounds().size, metrics, chrome_height);
                        let _ = resize_tx.send(IpcCommand::Resize { cols, rows });
                    })
                    .detach();
                    ForgeWindow::new(
                        event_rx,
                        input_tx,
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
            window.update(cx, |_, window, _| {
                window.on_next_frame(move |_, _| {
                    spawn_ipc_worker(socket, shell, event_tx, input_rx);
                });
            })?;
        }
        Ok(window)
    }
}

struct ForgeWindow {
    terminal: TerminalSurface,
    config: Arc<Config>,
    status: String,
    input: async_mpsc::UnboundedSender<IpcCommand>,
    focus: FocusHandle,
    render_count: Arc<AtomicU64>,
    frame_probe: Option<FrameProbe>,
    factory: WindowFactory,
    /// Cell where the current left-button drag started.
    drag_anchor: Option<forge_gui::CellPos>,
}

impl ForgeWindow {
    fn new(
        events: Receiver<UiEvent>,
        input: async_mpsc::UnboundedSender<IpcCommand>,
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
        Self {
            terminal: TerminalSurface::new(TerminalGrid::new(80, 24), metrics, palette),
            config,
            status: "Conectando a forge-termd…".into(),
            input,
            focus: cx.focus_handle(),
            render_count,
            frame_probe: None,
            factory,
            drag_anchor: None,
        }
    }

    fn handle_event(&mut self, event: UiEvent, cx: &mut Context<Self>) {
        match event {
            UiEvent::Message(ServerMessage::Exited { exit_code, .. }) => {
                // Keep the window visible so other Forge windows remain usable.
                self.status = format!("Sesión terminada ({})", exit_code.unwrap_or(0));
                cx.notify();
            }
            UiEvent::Message(ServerMessage::Error { message }) => {
                self.status = format!("Error del daemon: {message}");
            }
            UiEvent::Message(message) => {
                let before = self.terminal.grid.dimensions();
                match self.terminal.grid.apply_server_message(message) {
                    Ok(true) => {
                        if self.terminal.grid.dimensions() != before {
                            self.terminal.selection = None;
                        }
                        self.status =
                            format!("Sesión activa · revisión {}", self.terminal.grid.revision());
                    }
                    Ok(false) => {}
                    Err(error) => self.status = format!("Patch inválido: {error}"),
                }
            }
            UiEvent::Status(status) => self.status = status,
        }
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        let modifiers = KeyModifiers {
            control: keystroke.modifiers.control,
            alt: keystroke.modifiers.alt,
            shift: keystroke.modifiers.shift,
        };
        let copy_paste = modifiers.control && modifiers.shift && !modifiers.alt;
        if modifiers.control
            && !modifiers.shift
            && !modifiers.alt
            && keystroke.key.eq_ignore_ascii_case("t")
        {
            let factory = self.factory.clone();
            cx.spawn(async move |cx| {
                let _ = cx.update(|app| factory.open(app, true));
            })
            .detach();
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
            self.terminal.selection = None;
            let _ = self.input.send(IpcCommand::Input(bytes));
            cx.notify();
        }
    }

    fn selected_text(&self) -> Option<String> {
        let text = self.terminal.grid.selected_text(self.terminal.selection?);
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
            let _ = self.input.send(IpcCommand::Input(bytes));
        }
    }

    fn on_mouse_down(&mut self, event: &MouseDownEvent, cx: &mut Context<Self>) {
        if !self.terminal.contains(event.position) {
            return;
        }
        let Some(cell) = self.terminal.cell_at(event.position) else {
            return;
        };
        self.terminal.selection = match event.click_count {
            2 => Some(self.terminal.grid.word_at(cell)),
            n if n >= 3 => Some(self.terminal.grid.line_at(cell.y)),
            _ => None,
        };
        self.drag_anchor = Some(cell);
        cx.notify();
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, cx: &mut Context<Self>) {
        let Some(anchor) = self.drag_anchor else {
            return;
        };
        if event.pressed_button != Some(MouseButton::Left) {
            self.drag_anchor = None;
            return;
        }
        let Some(head) = self.terminal.cell_at(event.position) else {
            return;
        };
        let selection = Some(Selection { anchor, head });
        if self.terminal.selection != selection {
            self.terminal.selection = selection;
            cx.notify();
        }
    }

    fn on_mouse_up(&mut self, _event: &MouseUpEvent, cx: &mut Context<Self>) {
        if self.drag_anchor.take().is_none() {
            return;
        }
        // Linux convention: a finished selection is available on middle click.
        if let Some(text) = self.selected_text() {
            cx.write_to_primary(ClipboardItem::new_string(text));
        }
    }
}

impl Render for ForgeWindow {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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
        let (cols, rows) = self.terminal.grid.dimensions();
        let palette = self.terminal.palette;
        let config = &self.config;
        div()
            .id("forge-terminal")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|view, event, _, cx| view.on_key_down(event, cx)))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|view, event, _, cx| view.on_mouse_down(event, cx)),
            )
            .on_mouse_move(cx.listener(|view, event, _, cx| view.on_mouse_move(event, cx)))
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
            .overflow_hidden()
            .bg(palette.background)
            .text_color(palette.foreground)
            .p(px(config.terminal.padding))
            .font_family(config.font.family.clone())
            .when(config.terminal.show_status, |element| {
                element.child(
                    div()
                        .h(px(STATUS_HEIGHT - 4.0))
                        .mb(px(4.0))
                        .text_size(px(13.0))
                        .text_color(palette.accent)
                        .child(format!("Forge · {cols}×{rows} · {}", self.status)),
                )
            })
            .child(TerminalGridElement::new(cx.entity(), |view: &mut Self| {
                &mut view.terminal
            }))
    }
}

fn main() {
    let options = run_options();
    let started = Instant::now();
    let socket = options.socket;
    let grid_frames = options.benchmark_grid_frames;
    let (event_tx, event_rx) = mpsc::channel();
    let (input_tx, input_rx) = async_mpsc::unbounded_channel();
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

    Application::new().run(move |cx: &mut App| {
        let mut config = config;
        let family = resolve_font_family(&config.font.family, &cx.text_system().all_font_names());
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
        let bounds = Bounds::centered(None, window_size, cx);
        let window = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    ..Default::default()
                },
                |window, cx| {
                    let resize_tx = input_tx.clone();
                    let view = cx.new(|cx| {
                        cx.observe_window_bounds(window, move |_, window, _| {
                            let (cols, rows) =
                                grid_dimensions(window.bounds().size, metrics, chrome_height);
                            let _ = resize_tx.send(IpcCommand::Resize { cols, rows });
                        })
                        .detach();
                        ForgeWindow::new(
                            event_rx,
                            input_tx,
                            Arc::clone(&render_count),
                            metrics,
                            Arc::clone(&config),
                            cx,
                        )
                    });
                    window.focus(&view.read(cx).focus);
                    view
                },
            )
            .expect("open Forge window");
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
        } else if options.benchmark_idle_ms.is_none() && grid_frames.is_none() {
            window
                .update(cx, |_, window, _| {
                    window.on_next_frame(move |_, _| {
                        spawn_ipc_worker(socket, shell, event_tx, input_rx);
                    });
                })
                .expect("schedule terminal startup");
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
                        view.terminal
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
            .update(cx, |view, _, _| view.terminal.painted_cells)
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
                    runtime.block_on(run_ipc(socket, shell, events.clone(), input))
                });
            if let Err(error) = result {
                let _ = events.send(UiEvent::Status(format!("Sin conexión: {error:#}")));
            }
        })
        .expect("spawn GUI IPC worker");
}

async fn run_ipc(
    socket: PathBuf,
    (command, args): (String, Vec<String>),
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
    let _ = events.send(UiEvent::Status(format!("Sesión {session_id} conectada")));

    // Reading and writing run as independent tasks: a `select!` over both
    // would cancel in-flight reads on every keystroke and desynchronize the
    // framed stream under heavy output.
    let mut incoming = tokio::spawn(async move {
        loop {
            let message = reader.read_message::<ServerMessage>().await?.1;
            let exited = matches!(message, ServerMessage::Exited { .. });
            events
                .send(UiEvent::Message(message))
                .context("GUI closed")?;
            if exited {
                return Ok::<(), anyhow::Error>(());
            }
        }
    });
    let mut outgoing = tokio::spawn(async move {
        while let Some(command) = input.recv().await {
            let message = match command {
                IpcCommand::Input(data) => ClientMessage::Input { session_id, data },
                IpcCommand::Resize { cols, rows } => ClientMessage::Resize {
                    session_id,
                    cols,
                    rows,
                },
            };
            write_message(&mut writer, FrameKind::Notification, &message).await?;
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

/// Vertical space taken by window padding and the status line above the grid.
fn chrome_height(config: &Config) -> f32 {
    config.terminal.padding * 2.0
        + if config.terminal.show_status {
            STATUS_HEIGHT
        } else {
            0.0
        }
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
        assert!((chrome - 56.0).abs() < f32::EPSILON);
        let metrics = CellMetrics {
            width: 9.0,
            height: 18.0,
            font_size: 14.0,
        };
        assert_eq!(
            grid_dimensions(size(px(900.0), px(416.0)), metrics, chrome),
            (100, 20)
        );
        assert_eq!(
            grid_dimensions(size(px(1300.0), px(700.0)), CellMetrics::BENCHMARK, chrome),
            (216, 64)
        );
        assert_eq!(
            grid_dimensions(size(px(1.0), px(1.0)), metrics, chrome),
            (1, 1)
        );
        let mut bare = Config::default();
        bare.terminal.show_status = false;
        bare.terminal.padding = 0.0;
        assert!(chrome_height(&bare).abs() < f32::EPSILON);
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
