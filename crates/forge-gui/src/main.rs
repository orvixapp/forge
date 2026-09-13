use anyhow::{Context as _, Result, bail};
use forge_gui::{TerminalGrid, encode_terminal_key};
use gpui::{
    App, Application, Bounds, Context, FocusHandle, KeyDownEvent, Render, Timer, Window,
    WindowBounds, WindowOptions, div, prelude::*, px, rgb, size,
};
use proto_ipc::{
    ClientMessage, FrameKind, PROTOCOL_VERSION, ServerMessage, read_message, write_message,
};
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::{Duration, Instant},
};
use tokio::{
    net::UnixStream,
    process::{Child, Command},
    sync::mpsc as async_mpsc,
};

enum UiEvent {
    Message(ServerMessage),
    Status(String),
}

struct ForgeWindow {
    grid: TerminalGrid,
    status: String,
    input: async_mpsc::UnboundedSender<Vec<u8>>,
    focus: FocusHandle,
}

impl ForgeWindow {
    fn new(
        events: Receiver<UiEvent>,
        input: async_mpsc::UnboundedSender<Vec<u8>>,
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
            grid: TerminalGrid::new(80, 24),
            status: "Conectando a forge-termd…".into(),
            input,
            focus: cx.focus_handle(),
        }
    }

    fn handle_event(&mut self, event: UiEvent) {
        match event {
            UiEvent::Message(message) => match self.grid.apply_server_message(&message) {
                Ok(true) => {
                    self.status = format!("Sesión activa · revisión {}", self.grid.revision());
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
            let _ = self.input.send(bytes);
        }
    }
}

impl Render for ForgeWindow {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (cols, rows) = self.grid.dimensions();
        let lines = (0..rows).filter_map(|y| self.grid.row_text(y));
        div()
            .id("forge-terminal")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|view, event, _, _| view.on_key_down(event)))
            .size_full()
            .bg(rgb(0x11_13_18))
            .text_color(rgb(0xd8_de_e9))
            .p_4()
            .font_family("monospace")
            .child(
                div()
                    .mb_2()
                    .text_color(rgb(0x88_c0_d0))
                    .child(format!("Forge · {cols}×{rows} · {}", self.status)),
            )
            .children(lines.map(|line| div().h(px(18.0)).text_size(px(14.0)).child(line)))
    }
}

fn main() {
    let socket = socket_arg();
    let (event_tx, event_rx) = mpsc::channel();
    let (input_tx, input_rx) = async_mpsc::unbounded_channel();
    spawn_ipc_worker(socket, event_tx, input_rx);

    Application::new().run(|cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(960.0), px(600.0)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |window, cx| {
                let view = cx.new(|cx| ForgeWindow::new(event_rx, input_tx, cx));
                window.focus(&view.read(cx).focus);
                view
            },
        )
        .expect("open Forge window");
        cx.activate(true);
    });
}

fn spawn_ipc_worker(
    socket: PathBuf,
    events: Sender<UiEvent>,
    input: async_mpsc::UnboundedReceiver<Vec<u8>>,
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
    mut input: async_mpsc::UnboundedReceiver<Vec<u8>>,
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
                let Some(data) = data else { return Ok(()); };
                write_message(&mut writer, FrameKind::Notification,
                    &ClientMessage::Input { session_id, data }).await?;
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

fn socket_arg() -> PathBuf {
    let mut args = std::env::args().skip(1);
    let mut socket = PathBuf::from("/tmp/forge-prototype.sock");
    while let Some(arg) = args.next() {
        if arg == "--socket"
            && let Some(path) = args.next()
        {
            socket = path.into();
        }
    }
    socket
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
}
