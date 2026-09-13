mod grid_element;

use anyhow::{Context as _, Result, bail};
use forge_gui::{TerminalGrid, encode_terminal_key};
use gpui::{
    App, Application, Bounds, Context, FocusHandle, KeyDownEvent, Render, Timer, Window,
    WindowBounds, WindowHandle, WindowOptions, div, prelude::*, px, rgb, size,
};
use grid_element::{
    CellMetrics, DEFAULT_FOREGROUND, TerminalGridElement, TerminalSurface, WINDOW_BACKGROUND,
};
use proto_ipc::{
    ClientMessage, CursorStyle, FrameKind, PROTOCOL_VERSION, Rgb, ScreenCell, ScreenCursor,
    ScreenRow, ServerMessage, read_message, write_message,
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

struct ForgeWindow {
    terminal: TerminalSurface,
    status: String,
    input: async_mpsc::UnboundedSender<IpcCommand>,
    focus: FocusHandle,
    render_count: Arc<AtomicU64>,
    frame_probe: Option<FrameProbe>,
}

impl ForgeWindow {
    fn new(
        events: Receiver<UiEvent>,
        input: async_mpsc::UnboundedSender<IpcCommand>,
        render_count: Arc<AtomicU64>,
        metrics: CellMetrics,
        cx: &mut Context<Self>,
    ) -> Self {
        cx.spawn(async move |this, cx| {
            loop {
                Timer::after(Duration::from_millis(16)).await;
                let mut changed = false;
                while let Ok(event) = events.try_recv() {
                    if this.update(cx, |view, _| view.handle_event(event)).is_err() {
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
        Self {
            terminal: TerminalSurface::new(TerminalGrid::new(80, 24), metrics),
            status: "Conectando a forge-termd…".into(),
            input,
            focus: cx.focus_handle(),
            render_count,
            frame_probe: None,
        }
    }

    fn handle_event(&mut self, event: UiEvent) {
        match event {
            UiEvent::Message(message) => match self.terminal.grid.apply_server_message(message) {
                Ok(true) => {
                    self.status =
                        format!("Sesión activa · revisión {}", self.terminal.grid.revision());
                }
                Ok(false) => {}
                Err(error) => self.status = format!("Patch inválido: {error}"),
            },
            UiEvent::Status(status) => self.status = status,
        }
    }

    fn on_key_down(&mut self, event: &KeyDownEvent) {
        if let Some(bytes) = encode_terminal_key(
            &event.keystroke.key,
            event.keystroke.key_char.as_deref(),
            event.keystroke.modifiers.control,
        ) {
            let _ = self.input.send(IpcCommand::Input(bytes));
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
        div()
            .id("forge-terminal")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|view, event, _, _| view.on_key_down(event)))
            .size_full()
            .overflow_hidden()
            .bg(rgb(WINDOW_BACKGROUND))
            .text_color(rgb(DEFAULT_FOREGROUND))
            .p_4()
            .font_family("monospace")
            .child(
                div()
                    .mb_2()
                    .text_color(rgb(0x88_c0_d0))
                    .child(format!("Forge · {cols}×{rows} · {}", self.status)),
            )
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
    // The grid benchmark needs every one of its 200×60 cells on screen.
    let (metrics, window_size) = if grid_frames.is_some() {
        (CellMetrics::BENCHMARK, size(px(1300.0), px(700.0)))
    } else {
        (CellMetrics::DEFAULT, size(px(960.0), px(600.0)))
    };

    Application::new().run(move |cx: &mut App| {
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
                            let (cols, rows) = grid_dimensions(window.bounds().size, metrics);
                            let _ = resize_tx.send(IpcCommand::Resize { cols, rows });
                        })
                        .detach();
                        ForgeWindow::new(event_rx, input_tx, Arc::clone(&render_count), metrics, cx)
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
                        spawn_ipc_worker(socket, event_tx, input_rx);
                    });
                })
                .expect("schedule terminal startup");
        }
        if let Some(idle_ms) = options.benchmark_idle_ms {
            let count = Arc::clone(&render_count);
            cx.spawn(async move |cx| {
                Timer::after(Duration::from_secs(1)).await;
                let baseline = count.load(Ordering::Relaxed);
                Timer::after(Duration::from_millis(idle_ms)).await;
                emit_metrics(&GuiMetrics {
                    scenario: "idle",
                    elapsed_ms: Duration::from_millis(idle_ms).as_secs_f64() * 1_000.0,
                    samples_ms: None,
                    frames: count.load(Ordering::Relaxed).saturating_sub(baseline),
                    pss_kib: process_pss_kib(),
                    painted_cells: None,
                });
                let _ = cx.update(|cx| cx.quit());
            })
            .detach();
        }
        if let Some(iterations) = grid_frames {
            spawn_grid_benchmark(iterations, window, cx);
        }
        cx.activate(true);
    });
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
                .and_then(|runtime| runtime.block_on(run_ipc(socket, events.clone(), input)));
            if let Err(error) = result {
                let _ = events.send(UiEvent::Status(format!("Sin conexión: {error:#}")));
            }
        })
        .expect("spawn GUI IPC worker");
}

async fn run_ipc(
    socket: PathBuf,
    events: Sender<UiEvent>,
    mut input: async_mpsc::UnboundedReceiver<IpcCommand>,
) -> Result<()> {
    let (stream, _daemon) = connect_or_start_daemon(&socket).await?;
    let (mut reader, mut writer) = stream.into_split();
    write_message(
        &mut writer,
        FrameKind::Request,
        &ClientMessage::Initialize {
            protocol_version: PROTOCOL_VERSION,
            client_name: "forge-gui".into(),
        },
    )
    .await?;
    match read_message::<_, ServerMessage>(&mut reader).await?.1 {
        ServerMessage::Initialized { protocol_version } if protocol_version == PROTOCOL_VERSION => {
        }
        message => bail!("respuesta initialize inesperada: {message:?}"),
    }
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    write_message(
        &mut writer,
        FrameKind::Request,
        &ClientMessage::CreateSession {
            request_id: 1,
            command: shell,
            args: Vec::new(),
            cols: 80,
            rows: 24,
        },
    )
    .await?;
    let session_id = match read_message::<_, ServerMessage>(&mut reader).await?.1 {
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

    loop {
        tokio::select! {
            data = input.recv() => {
                let Some(command) = data else { return Ok(()); };
                let message = match command {
                    IpcCommand::Input(data) => ClientMessage::Input { session_id, data },
                    IpcCommand::Resize { cols, rows } => ClientMessage::Resize {
                        session_id, cols, rows,
                    },
                };
                write_message(&mut writer, FrameKind::Notification, &message).await?;
            }
            message = read_message::<_, ServerMessage>(&mut reader) => {
                let message = message?.1;
                let exited = matches!(message, ServerMessage::Exited { .. });
                events.send(UiEvent::Message(message)).context("GUI closed")?;
                if exited { return Ok(()); }
            }
        }
    }
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

/// Vertical space taken by window padding and the status line above the grid.
const CHROME_HEIGHT: f32 = 56.0;

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn grid_dimensions(size: gpui::Size<gpui::Pixels>, metrics: CellMetrics) -> (u16, u16) {
    let cols = ((size.width / px(metrics.width)).floor() as u16).clamp(1, 500);
    let rows =
        (((size.height - px(CHROME_HEIGHT)) / px(metrics.height)).floor() as u16).clamp(1, 300);
    (cols, rows)
}

struct RunOptions {
    socket: PathBuf,
    exit_after_first_frame: bool,
    benchmark_idle_ms: Option<u64>,
    benchmark_grid_frames: Option<usize>,
}

fn run_options() -> RunOptions {
    let mut args = std::env::args().skip(1);
    let mut socket = PathBuf::from("/tmp/forge-prototype.sock");
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
        assert_eq!(
            grid_dimensions(size(px(900.0), px(416.0)), CellMetrics::DEFAULT),
            (100, 20)
        );
        assert_eq!(
            grid_dimensions(size(px(1300.0), px(700.0)), CellMetrics::BENCHMARK),
            (216, 64)
        );
        assert_eq!(
            grid_dimensions(size(px(1.0), px(1.0)), CellMetrics::DEFAULT),
            (1, 1)
        );
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
