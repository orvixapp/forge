//! Connection between one terminal tab and `forge-termd`.
//!
//! Each tab owns a worker thread with its own Tokio runtime. Reading and
//! writing run as independent tasks: a `select!` over both would cancel
//! in-flight reads on every keystroke and desynchronize the framed stream
//! under heavy output.

use proto_ipc::ServerMessage;
use std::{path::PathBuf, sync::mpsc::Sender};
use tokio::sync::mpsc as async_mpsc;

// Without a daemon (non-Unix builds) the messages are only ever sent, never
// produced, so the compiler sees the payloads as unused.
#[cfg_attr(not(unix), allow(dead_code))]
pub enum UiEvent {
    Message { tab_id: u64, message: ServerMessage },
    Status { tab_id: u64, status: String },
}

#[cfg_attr(not(unix), allow(dead_code))]
pub enum IpcCommand {
    Input(Vec<u8>),
    Resize { cols: u16, rows: u16 },
}

/// Everything needed to create one daemon session.
#[derive(Clone)]
#[cfg_attr(not(unix), allow(dead_code))]
pub struct SessionSpec {
    pub socket: PathBuf,
    pub command: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
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
        write_message(
            &mut writer,
            FrameKind::Request,
            &ClientMessage::CreateSession {
                request_id: 1,
                command: spec.command,
                args: spec.args,
                cwd: spec.cwd,
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
        Ok((reader, writer, session_id, daemon))
    }

    pub async fn run_ipc(
        spec: SessionSpec,
        tab_id: u64,
        events: Sender<UiEvent>,
        mut input: async_mpsc::UnboundedReceiver<IpcCommand>,
    ) -> Result<()> {
        let (mut reader, mut writer, session_id, _daemon) = handshake(spec).await?;
        let _ = events.send(UiEvent::Status {
            tab_id,
            status: format!("Sesión {session_id} conectada"),
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
