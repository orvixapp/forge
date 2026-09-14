//! Connection between one terminal tab and `forge-termd`.
//!
//! Each tab owns a worker thread with its own Tokio runtime. Reading and
//! writing run as independent tasks: a `select!` over both would cancel
//! in-flight reads on every keystroke and desynchronize the framed stream
//! under heavy output.

use forge_gui::i18n::trf;
use proto_ipc::{
    KeyEvent, MouseEvent, ProcessSignal, PromptDirection, ScrollRequest, ServerMessage,
};
use std::{path::PathBuf, sync::mpsc::Sender};
use tokio::sync::mpsc as async_mpsc;

// Without a daemon (other platforms) the messages are only ever sent, never
// produced, so the compiler sees the payloads as unused.
#[cfg_attr(not(any(unix, windows)), allow(dead_code))]
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
        daemon_instance: u64,
    },
    /// The workspace path index was (re)built.
    IndexReady {
        index: Box<Result<forge_project::PathIndex, String>>,
    },
    /// A gutter diff against `HEAD` finished for an editor tab.
    GitDiff {
        tab_id: u64,
        version: u64,
        /// Repository, `HEAD` text (`None` when untracked) and the diff.
        state: Box<GitDiffResult>,
    },
    AgentConnected {
        tab_id: u64,
        session_id: String,
    },
    AgentEvent {
        tab_id: u64,
        event: proto_acp::AcpEvent,
    },
    AgentStatus {
        tab_id: u64,
        status: String,
    },
    /// The outstanding `session/prompt` request completed. Session updates
    /// stream independently while that request is in flight.
    AgentTurnFinished {
        tab_id: u64,
        error: Option<String>,
    },
    /// An agent-to-client ACP request that must inspect or mutate live GUI state.
    AgentRequest {
        tab_id: u64,
        message: Box<proto_acp::JsonRpcMessage>,
        response: tokio::sync::oneshot::Sender<proto_acp::JsonRpcMessage>,
    },
}

pub struct GitDiffResult {
    pub info: Option<forge_git::RepoInfo>,
    pub head: Option<String>,
    pub diff: forge_git::LineDiff,
}

#[cfg_attr(not(any(unix, windows)), allow(dead_code))]
pub enum IpcCommand {
    /// Kill the program behind the tab (the user closed it).
    Shutdown,
    Key(KeyEvent),
    Mouse(MouseEvent),
    Paste(String),
    Scroll(ScrollRequest),
    ScrollToPrompt(PromptDirection),
    Signal(ProcessSignal),
    /// Scrollback search; the daemon answers with `SearchResults` carrying
    /// the same `request_id`.
    Search {
        request_id: u64,
        query: String,
        regex: bool,
        case_sensitive: bool,
    },
    Resize {
        cols: u16,
        rows: u16,
    },
}

/// Everything needed to create one daemon session, or to reattach to one
/// that survived a previous Forge process.
#[derive(Clone)]
#[cfg_attr(not(any(unix, windows)), allow(dead_code))]
pub struct SessionSpec {
    pub socket: PathBuf,
    pub command: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    /// Initial PTY size. Starting at the real window dimensions avoids
    /// reflowing the shell's first prompt from the historical 80x24 default.
    pub cols: u16,
    pub rows: u16,
    /// Extra environment for the shell (shell integration, `TERM_PROGRAM`).
    pub env: Vec<(String, String)>,
    /// Daemon session from the saved layout; a new one is created when the
    /// daemon no longer has it.
    pub attach: Option<u64>,
    /// Instance that issued `attach`; IDs from another daemon are stale.
    pub daemon_instance: Option<u64>,
}

#[cfg(any(unix, windows))]
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
                    runtime.block_on(client::run_ipc(spec, tab_id, events.clone(), input))
                });
            if let Err(error) = result {
                let _ = events.send(UiEvent::Status {
                    tab_id,
                    status: trf("No connection: {}", &[&format!("{error:#}")]),
                });
            }
        })
        .expect("spawn GUI IPC worker");
}

#[cfg(not(any(unix, windows)))]
#[allow(clippy::needless_pass_by_value)]
pub fn spawn_ipc_worker(
    _spec: SessionSpec,
    tab_id: u64,
    events: Sender<UiEvent>,
    _input: async_mpsc::UnboundedReceiver<IpcCommand>,
) {
    let _ = events.send(UiEvent::Status {
        tab_id,
        status: forge_gui::i18n::tr("forge-termd is not available on this platform yet").into(),
    });
}

/// The client side of the daemon transport: a Unix socket, or a named pipe
/// on Windows. Both are plain `AsyncRead + AsyncWrite` streams from here.
#[cfg(any(unix, windows))]
mod client {
    use super::{IpcCommand, SessionSpec, UiEvent, trf};
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
        io::{ReadHalf, WriteHalf},
        process::{Child, Command},
        sync::mpsc as async_mpsc,
    };

    #[cfg(unix)]
    type Stream = tokio::net::UnixStream;
    #[cfg(windows)]
    type Stream = tokio::net::windows::named_pipe::NamedPipeClient;

    #[cfg(unix)]
    async fn connect(socket: &Path) -> std::io::Result<Stream> {
        tokio::net::UnixStream::connect(socket).await
    }

    /// Named pipes report "busy" while the server recreates its instance
    /// between clients; wait briefly instead of failing.
    #[cfg(windows)]
    async fn connect(socket: &Path) -> std::io::Result<Stream> {
        use tokio::net::windows::named_pipe::ClientOptions;
        const ERROR_PIPE_BUSY: i32 = 231;
        let name = pipe_name(socket);
        for _ in 0..50 {
            match ClientOptions::new().open(&name) {
                Ok(client) => return Ok(client),
                Err(error) if error.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Err(error) => return Err(error),
            }
        }
        Err(std::io::Error::other("named pipe stayed busy"))
    }

    /// Mirrors the daemon's mapping from a socket path to a pipe name.
    #[cfg(windows)]
    fn pipe_name(path: &Path) -> String {
        let text = path.to_string_lossy();
        if text.starts_with(r"\\.\pipe\") {
            return text.into_owned();
        }
        let stem = path.file_name().map_or_else(
            || "forge-termd".into(),
            |name| name.to_string_lossy().replace(['\\', '/', ':'], "-"),
        );
        format!(r"\\.\pipe\{stem}")
    }

    /// Tabs initialize on separate worker threads. Serialize protocol recovery
    /// so two tabs cannot unlink the replacement socket at the same time.
    static PROTOCOL_RECOVERY: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    #[derive(Debug)]
    struct ProtocolMismatch(String);

    impl std::fmt::Display for ProtocolMismatch {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str(&self.0)
        }
    }

    impl std::error::Error for ProtocolMismatch {}

    /// Connects (starting the daemon if needed), creates the session and
    /// attaches. Returns the framed reader, the writer and the session id.
    type Connection = (
        FrameReader<ReadHalf<Stream>>,
        WriteHalf<Stream>,
        u64,
        u64,
        Option<DaemonGuard>,
    );

    async fn handshake(spec: SessionSpec) -> Result<Connection> {
        match handshake_once(spec.clone()).await {
            Ok(connection) => return Ok(connection),
            Err(error) if error.downcast_ref::<ProtocolMismatch>().is_some() => {}
            Err(error) => return Err(error),
        }

        let _recovery = PROTOCOL_RECOVERY.lock().await;
        // Another tab may have completed recovery while this worker waited.
        match handshake_once(spec.clone()).await {
            Ok(connection) => return Ok(connection),
            Err(error) if error.downcast_ref::<ProtocolMismatch>().is_some() => {}
            Err(error) => return Err(error),
        }

        // A daemon speaking an old protocol is left to die with its last
        // client; on Unix its socket file is taken over right away.
        #[cfg(unix)]
        match std::fs::remove_file(&spec.socket) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("remove the incompatible daemon socket"),
        }
        handshake_once(spec)
            .await
            .context("restart forge-termd after the protocol update")
    }

    async fn handshake_once(spec: SessionSpec) -> Result<Connection> {
        let (stream, daemon) = connect_or_start_daemon(&spec.socket).await?;
        let (reader, mut writer) = tokio::io::split(stream);
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
        let daemon_instance = match reader.read_message::<ServerMessage>().await?.1 {
            ServerMessage::Initialized {
                protocol_version,
                daemon_instance,
            } if protocol_version == PROTOCOL_VERSION => daemon_instance,
            ServerMessage::Error { message } if message.contains("protocol mismatch") => {
                return Err(ProtocolMismatch(message).into());
            }
            message => bail!("respuesta initialize inesperada: {message:?}"),
        };
        let existing = match spec
            .attach
            .filter(|_| spec.daemon_instance == Some(daemon_instance))
        {
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
                    env: spec.env,
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
        Ok((reader, writer, session_id, daemon_instance, daemon))
    }

    pub async fn run_ipc(
        spec: SessionSpec,
        tab_id: u64,
        events: Sender<UiEvent>,
        mut input: async_mpsc::UnboundedReceiver<IpcCommand>,
    ) -> Result<()> {
        let wanted = spec.attach;
        let expected_daemon = spec.daemon_instance;
        let (mut reader, mut writer, session_id, daemon_instance, _daemon) =
            handshake(spec).await?;
        let _ = events.send(UiEvent::Attached {
            tab_id,
            session_id,
            daemon_instance,
        });
        let _ = events.send(UiEvent::Status {
            tab_id,
            status: if wanted == Some(session_id) && expected_daemon == Some(daemon_instance) {
                trf("Session {} recovered", &[&session_id])
            } else {
                trf("Session {} connected", &[&session_id])
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
                    | IpcCommand::Scroll(_)
                    | IpcCommand::ScrollToPrompt(_)
                    | IpcCommand::Signal(_)
                    | IpcCommand::Search { .. } => {
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
            IpcCommand::ScrollToPrompt(direction) => ClientMessage::ScrollToPrompt {
                session_id,
                direction,
            },
            IpcCommand::Signal(signal) => ClientMessage::Signal { session_id, signal },
            IpcCommand::Search {
                request_id,
                query,
                regex,
                case_sensitive,
            } => ClientMessage::Search {
                session_id,
                request_id,
                query,
                regex,
                case_sensitive,
            },
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

    async fn connect_or_start_daemon(socket: &Path) -> Result<(Stream, Option<DaemonGuard>)> {
        if let Ok(stream) = connect(socket).await {
            return Ok((stream, None));
        }
        let ghostty = ghostty_library();
        if !ghostty.is_file() {
            bail!(
                "{} does not exist; run ./scripts/bootstrap-ghostty.sh once",
                ghostty.display()
            );
        }
        let mut command = daemon_command(socket, &ghostty)?;
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .kill_on_drop(false);
        // Its own process group, so a Ctrl+C aimed at the GUI's terminal
        // does not take the daemon (and every shell) down with it.
        #[cfg(unix)]
        command.process_group(0);
        let child = command.spawn().context("start forge-termd automatically")?;
        let mut daemon = DaemonGuard(child);
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            match connect(socket).await {
                Ok(stream) => return Ok((stream, Some(daemon))),
                Err(_) if Instant::now() < deadline => {
                    if let Some(status) = daemon.0.try_wait().context("consultar forge-termd")? {
                        // Several tabs may race to start the daemon; the
                        // losers exit because the socket is taken, and the
                        // winner is the one to connect to.
                        if let Ok(stream) = connect(socket).await {
                            return Ok((stream, None));
                        }
                        bail!("forge-termd exited during startup: {status}");
                    }
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
                Err(error) => bail!("forge-termd did not open {}: {error}", socket.display()),
            }
        }
    }

    fn daemon_command(socket: &Path, ghostty: &Path) -> Result<Command> {
        let sibling = std::env::current_exe()
            .context("resolver ejecutable actual")?
            .with_file_name(if cfg!(windows) {
                "proto-termd.exe"
            } else {
                "proto-termd"
            });
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

    /// `libghostty-vt` built by `scripts/bootstrap-ghostty.sh`, unless
    /// `FORGE_GHOSTTY_LIB` points elsewhere.
    pub fn ghostty_library() -> PathBuf {
        let file = if cfg!(target_os = "macos") {
            "libghostty-vt.dylib"
        } else if cfg!(windows) {
            "ghostty-vt.dll"
        } else {
            "libghostty-vt.so"
        };
        std::env::var_os("FORGE_GHOSTTY_LIB").map_or_else(
            || workspace_dir().join("target/ghostty/lib").join(file),
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
                assert!(ghostty_library().starts_with(workspace_dir().join("target/ghostty/lib")));
            }
        }
    }
}
