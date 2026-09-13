#[cfg(not(unix))]
fn main() {
    eprintln!("proto-termd currently implements Unix sockets only; Windows named pipes are next");
    std::process::exit(2);
}

#[cfg(unix)]
mod unix {
    use anyhow::{Context, Result, bail};
    use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
    use proto_ghostty_vt::{DirtyState, GhosttyLibrary, GhosttyTerminal};
    use proto_ipc::{
        ClientMessage, FrameKind, PROTOCOL_VERSION, ServerMessage, read_message, write_message,
    };
    use std::{
        collections::{HashMap, VecDeque},
        io::{Read, Write},
        os::unix::fs::PermissionsExt,
        path::PathBuf,
        sync::{Arc, Mutex, mpsc},
        thread,
    };
    use tokio::{
        net::{UnixListener, UnixStream},
        sync::{RwLock, broadcast, mpsc as async_mpsc},
    };
    use tracing::{error, info, warn};

    const BACKLOG_LIMIT: usize = 4 * 1024 * 1024;
    const CONNECTION_QUEUE: usize = 256;

    #[derive(Debug, Clone)]
    enum SessionEvent {
        Output(Vec<u8>),
        ScreenUpdated {
            revision: u64,
            cols: u16,
            rows: u16,
            text: String,
        },
        Exited(Option<u32>),
    }

    struct Session {
        input: mpsc::SyncSender<Vec<u8>>,
        master: Mutex<Box<dyn MasterPty + Send>>,
        child: Mutex<Box<dyn Child + Send + Sync>>,
        backlog: Mutex<VecDeque<u8>>,
        terminal: Mutex<GhosttyTerminal>,
        cols: std::sync::atomic::AtomicU16,
        rows: std::sync::atomic::AtomicU16,
        revision: std::sync::atomic::AtomicU64,
        exit_code: Mutex<Option<u32>>,
        events: broadcast::Sender<SessionEvent>,
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
                .resize(cols, rows)
                .context("resize Ghostty terminal")?;
            self.emit_screen()
        }

        fn shutdown(&self) -> Result<()> {
            self.child
                .lock()
                .expect("child mutex poisoned")
                .kill()
                .context("kill PTY child")
        }

        fn feed_vt(&self, data: &[u8]) -> Result<()> {
            self.terminal
                .lock()
                .expect("terminal mutex poisoned")
                .write(data);
            self.emit_screen()
        }

        fn emit_screen(&self) -> Result<()> {
            let frame = self
                .terminal
                .lock()
                .expect("terminal mutex poisoned")
                .render_snapshot()
                .context("update Ghostty render state")?;
            if frame.dirty == DirtyState::Clean {
                return Ok(());
            }
            let revision = self
                .revision
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                + 1;
            let _ = self.events.send(SessionEvent::ScreenUpdated {
                revision,
                cols: frame.cols,
                rows: frame.rows,
                text: frame.text.unwrap_or_default(),
            });
            Ok(())
        }
    }

    struct Daemon {
        ghostty: GhosttyLibrary,
        sessions: RwLock<HashMap<u64, Arc<Session>>>,
        next_session_id: std::sync::atomic::AtomicU64,
    }

    impl Daemon {
        fn new(ghostty: GhosttyLibrary) -> Self {
            Self {
                ghostty,
                sessions: RwLock::new(HashMap::new()),
                next_session_id: std::sync::atomic::AtomicU64::new(0),
            }
        }

        async fn create_session(
            &self,
            command: String,
            args: Vec<String>,
            cols: u16,
            rows: u16,
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
            for arg in args {
                builder.arg(arg);
            }
            let child = pair.slave.spawn_command(builder).context("spawn command")?;
            drop(pair.slave);
            let reader = pair.master.try_clone_reader().context("clone PTY reader")?;
            let writer = pair.master.take_writer().context("take PTY writer")?;
            let (input_tx, input_rx) = mpsc::sync_channel::<Vec<u8>>(128);
            let (events, _) = broadcast::channel(256);
            let terminal = self
                .ghostty
                .terminal(cols, rows)
                .context("create Ghostty terminal")?;
            let session = Arc::new(Session {
                input: input_tx,
                master: Mutex::new(pair.master),
                child: Mutex::new(child),
                backlog: Mutex::new(VecDeque::with_capacity(64 * 1024)),
                terminal: Mutex::new(terminal),
                cols: std::sync::atomic::AtomicU16::new(cols.max(1)),
                rows: std::sync::atomic::AtomicU16::new(rows.max(1)),
                revision: std::sync::atomic::AtomicU64::new(0),
                exit_code: Mutex::new(None),
                events,
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
                let mut chunk = vec![0_u8; 16 * 1024];
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
        if socket.exists() {
            if UnixStream::connect(&socket).await.is_ok() {
                bail!(
                    "another terminal daemon is already listening at {}",
                    socket.display()
                );
            }
            std::fs::remove_file(&socket).context("remove stale socket")?;
        }
        let listener =
            UnixListener::bind(&socket).with_context(|| format!("bind {}", socket.display()))?;
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))
            .context("restrict socket permissions")?;
        info!(path = %socket.display(), "terminal daemon listening");
        let daemon = Arc::new(Daemon::new(ghostty));
        loop {
            let (stream, _) = listener.accept().await?;
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

    async fn handle_connection(stream: UnixStream, daemon: Arc<Daemon>) -> Result<()> {
        let (mut reader, mut writer) = stream.into_split();
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
                cols,
                rows,
            } => {
                let session_id = daemon.create_session(command, args, cols, rows).await?;
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
            ClientMessage::Detach { .. } => {
                // Attach forwarding tasks end when this connection closes. Per-session
                // detach tokens arrive in the next protocol iteration.
            }
            ClientMessage::Initialize { .. } => bail!("connection is already initialized"),
        }
        Ok(())
    }

    async fn attach_session(
        session_id: u64,
        session: Arc<Session>,
        out_tx: &async_mpsc::Sender<(FrameKind, ServerMessage)>,
    ) -> Result<()> {
        let mut events = session.events.subscribe();
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

        let forwarding = out_tx.clone();
        tokio::spawn(async move {
            loop {
                let message = match events.recv().await {
                    Ok(SessionEvent::Output(data)) => ServerMessage::Output { session_id, data },
                    Ok(SessionEvent::ScreenUpdated {
                        revision,
                        cols,
                        rows,
                        text,
                    }) => ServerMessage::ScreenUpdated {
                        session_id,
                        revision,
                        cols,
                        rows,
                        text,
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
        Ok(())
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

#[cfg(unix)]
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    unix::run().await
}
