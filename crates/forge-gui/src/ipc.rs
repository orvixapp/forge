//! Connection between one terminal tab and `forge-termd`.
//!
//! Each tab owns a worker thread with its own Tokio runtime. Reading and
//! writing run as independent tasks: a `select!` over both would cancel
//! in-flight reads on every keystroke and desynchronize the framed stream
//! under heavy output.

use proto_ipc::{KeyEvent, MouseEvent, ScrollRequest, ServerMessage};
use std::{path::PathBuf, sync::mpsc::Sender};
use tokio::sync::mpsc as async_mpsc;

// Without a daemon (non-Unix builds) the messages are only ever sent, never
// produced, so the compiler sees the payloads as unused.
#[cfg_attr(not(unix), allow(dead_code))]
pub enum UiEvent {
    Message {
        tab_id: u64,
        message: ServerMessage,
    },
    Status {
        tab_id: u64,
        status: String,
    },
    /// The daemon session a tab is attached to, for `session.json`.
    Attached {
        tab_id: u64,
        session_id: u64,
    },
}

#[cfg_attr(not(unix), allow(dead_code))]
pub enum IpcCommand {
    /// Kill the program behind the tab (the user closed it).
    Shutdown,
    Key(KeyEvent),
    Mouse(MouseEvent),
    Paste(String),
    Scroll(ScrollRequest),
    Resize {
        cols: u16,
        rows: u16,
    },
}

/// Everything needed to create one daemon session, or to reattach to one
/// that survived a previous Forge process.
#[derive(Clone)]
#[cfg_attr(not(unix), allow(dead_code))]
pub struct SessionSpec {
    pub socket: PathBuf,
    pub command: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    /// Initial PTY size. Starting at the real window dimensions avoids
    /// reflowing the shell's first prompt from the historical 80x24 default.
    pub cols: u16,
    pub rows: u16,
    /// Daemon session from the saved layout; a new one is created when the
    /// daemon no longer has it.
    pub attach: Option<u64>,
}

#[cfg(unix)]
pub fn spawn_ipc_worker(
    spec: SessionSpec,
    tab_id: u64,
    events: Sender<UiEvent>,
    input: async_mpsc::UnboundedReceiver<IpcCommand>,
) {
    std::thread::Builder::new()
        .name(format!("forge-gui-ipc-{tab_id}"))
        .spawn(move || {
            let result = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(anyhow::Error::from)
                .and_then(|runtime| {
                    runtime.block_on(unix::run_ipc(spec, tab_id, events.clone(), input))
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

/// Phase 1 targets Windows as "build only": the daemon needs `ConPTY` and
/// named pipes, which arrive with the terminal phase.
#[cfg(not(unix))]
#[allow(clippy::needless_pass_by_value)]
pub fn spawn_ipc_worker(
    _spec: SessionSpec,
    tab_id: u64,
    events: Sender<UiEvent>,
    _input: async_mpsc::UnboundedReceiver<IpcCommand>,
) {
    let _ = events.send(UiEvent::Status {
        tab_id,
        status: "forge-termd no está disponible en esta plataforma todavía".into(),
    });
}

#[cfg(unix)]
mod unix {
    use super::{IpcCommand, SessionSpec, UiEvent};
    use anyhow::{Context as _, Result, bail};
    use proto_ipc::{
        ClientMessage, FrameKind, FrameReader, PROTOCOL_VERSION, ServerMessage, write_message,
    };
    use std::{
        path::{Path, PathBuf},
        process::Stdio,
        sync::mpsc::Sender,
        time::{Duration, Instant},
    };
    use tokio::{
        net::UnixStream,
        process::{Child, Command},
        sync::mpsc as async_mpsc,
    };

    /// Connects (starting the daemon if needed), creates the session and
    /// attaches. Returns the framed reader, the writer and the session id.
    async fn handshake(
        spec: SessionSpec,
    ) -> Result<(
        FrameReader<tokio::net::unix::OwnedReadHalf>,
        tokio::net::unix::OwnedWriteHalf,
        u64,
        Option<DaemonGuard>,
    )> {
        let (stream, daemon) = connect_or_start_daemon(&spec.socket).await?;
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
            ServerMessage::Initialized { protocol_version }
                if protocol_version == PROTOCOL_VERSION => {}
            message => bail!("respuesta initialize inesperada: {message:?}"),
        }
        let existing = match spec.attach {
            Some(wanted) => {
                write_message(
                    &mut writer,
                    FrameKind::Request,
                    &ClientMessage::ListSessions,
                )
                .await?;
                match reader.read_message::<ServerMessage>().await?.1 {
                    ServerMessage::Sessions { sessions } => sessions
                        .iter()
                        .find(|session| session.session_id == wanted && session.alive)
                        .map(|session| session.session_id),
                    message => bail!("respuesta list_sessions inesperada: {message:?}"),
                }
            }
            None => None,
        };
        let session_id = if let Some(session_id) = existing {
            session_id
        } else {
            write_message(
                &mut writer,
                FrameKind::Request,
                &ClientMessage::CreateSession {
                    request_id: 1,
                    command: spec.command,
                    args: spec.args,
                    cwd: spec.cwd,
                    cols: spec.cols,
                    rows: spec.rows,
                },
            )
            .await?;
            match reader.read_message::<ServerMessage>().await?.1 {
                ServerMessage::SessionCreated { session_id, .. } => session_id,
                message => bail!("respuesta create_session inesperada: {message:?}"),
            }
        };
        write_message(
            &mut writer,
            FrameKind::Request,
            &ClientMessage::Attach { session_id },
        )
        .await?;
        Ok((reader, writer, session_id, daemon))
    }

    pub async fn run_ipc(
        spec: SessionSpec,
        tab_id: u64,
        events: Sender<UiEvent>,
        mut input: async_mpsc::UnboundedReceiver<IpcCommand>,
    ) -> Result<()> {
        let wanted = spec.attach;
        let (mut reader, mut writer, session_id, _daemon) = handshake(spec).await?;
        let _ = events.send(UiEvent::Attached { tab_id, session_id });
        let _ = events.send(UiEvent::Status {
            tab_id,
            status: if wanted == Some(session_id) {
                format!("Sesión {session_id} recuperada")
            } else {
                format!("Sesión {session_id} conectada")
            },
        });

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
                    IpcCommand::Shutdown => {
                        write_message(
                            &mut writer,
                            FrameKind::Request,
                            &ClientMessage::ShutdownSession { session_id },
                        )
                        .await?;
                        return Ok::<(), anyhow::Error>(());
                    }
                    IpcCommand::Key(_)
                    | IpcCommand::Mouse(_)
                    | IpcCommand::Paste(_)
                    | IpcCommand::Scroll(_) => {
                        write_message(
                            &mut writer,
                            FrameKind::Notification,
                            &client_message(session_id, command),
                        )
                        .await?;
                    }
                    IpcCommand::Resize { cols, rows } => {
                        let mut latest = (cols, rows);
                        // Interactive window resize can generate hundreds of
                        // bounds updates. A PTY reflow is expensive and each
                        // one produces a screen snapshot, so keep only the
                        // final cell dimensions after a short quiet period.
                        loop {
                            match tokio::time::timeout(Duration::from_millis(60), input.recv())
                                .await
                            {
                                Ok(Some(IpcCommand::Resize { cols, rows })) => {
                                    latest = (cols, rows);
                                }
                                Ok(Some(other)) => {
                                    send_resize(&mut writer, session_id, latest).await?;
                                    write_message(
                                        &mut writer,
                                        FrameKind::Notification,
                                        &client_message(session_id, other),
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

    /// The wire form of a non-resize command.
    fn client_message(session_id: u64, command: IpcCommand) -> ClientMessage {
        match command {
            IpcCommand::Shutdown => ClientMessage::ShutdownSession { session_id },
            IpcCommand::Key(event) => ClientMessage::Key { session_id, event },
            IpcCommand::Mouse(event) => ClientMessage::Mouse { session_id, event },
            IpcCommand::Paste(text) => ClientMessage::Paste { session_id, text },
            IpcCommand::Scroll(scroll) => ClientMessage::Scroll { session_id, scroll },
            IpcCommand::Resize { cols, rows } => ClientMessage::Resize {
                session_id,
                cols,
                rows,
            },
        }
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

    /// The daemon Forge started. It is deliberately **not** killed when the
    /// GUI exits: surviving the UI is the daemon's reason to exist (§14.2).
    struct DaemonGuard(Child);

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
            .kill_on_drop(false)
            .process_group(0)
            .spawn()
            .context("arrancar forge-termd automáticamente")?;
        let mut daemon = DaemonGuard(child);
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            match UnixStream::connect(socket).await {
                Ok(stream) => return Ok((stream, Some(daemon))),
                Err(_) if Instant::now() < deadline => {
                    if let Some(status) = daemon.0.try_wait().context("consultar forge-termd")? {
                        // Several tabs may race to start the daemon; the
                        // losers exit because the socket is taken, and the
                        // winner is the one to connect to.
                        if let Ok(stream) = UnixStream::connect(socket).await {
                            return Ok((stream, None));
                        }
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

    pub fn workspace_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .expect("forge-gui crate belongs to workspace/crates")
            .to_path_buf()
    }

    pub fn ghostty_library() -> PathBuf {
        std::env::var_os("FORGE_GHOSTTY_LIB").map_or_else(
            || workspace_dir().join("target/ghostty/lib/libghostty-vt.so"),
            PathBuf::from,
        )
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
    }
}
