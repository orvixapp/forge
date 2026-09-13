#[cfg(not(unix))]
fn main() {
    eprintln!("proto-termd currently implements Unix sockets only; Windows named pipes are next");
    std::process::exit(2);
}

#[cfg(unix)]
mod unix {
    use anyhow::{Context, Result, bail};
    use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
    use proto_ipc::{
        ClientMessage, FrameKind, PROTOCOL_VERSION, ServerMessage, read_message, write_message,
    };
    use std::{
        collections::{HashMap, VecDeque},
        io::{Read, Write},
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
        Exited(Option<u32>),
    }

    struct Session {
        input: mpsc::SyncSender<Vec<u8>>,
        master: Mutex<Box<dyn MasterPty + Send>>,
        child: Mutex<Box<dyn Child + Send + Sync>>,
        backlog: Mutex<VecDeque<u8>>,
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
                .context("resize PTY")
        }

        fn shutdown(&self) -> Result<()> {
            self.child
                .lock()
                .expect("child mutex poisoned")
                .kill()
                .context("kill PTY child")
        }
    }

    #[derive(Default)]
    struct Daemon {
        sessions: RwLock<HashMap<u64, Arc<Session>>>,
        next_session_id: std::sync::atomic::AtomicU64,
    }

    impl Daemon {
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
            let mut reader = pair.master.try_clone_reader().context("clone PTY reader")?;
            let mut writer = pair.master.take_writer().context("take PTY writer")?;
            let (input_tx, input_rx) = mpsc::sync_channel::<Vec<u8>>(128);
            let (events, _) = broadcast::channel(256);
            let session = Arc::new(Session {
                input: input_tx,
                master: Mutex::new(pair.master),
                child: Mutex::new(child),
                backlog: Mutex::new(VecDeque::with_capacity(64 * 1024)),
                events,
            });
            let session_id = self
                .next_session_id
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                + 1;

            let read_session = Arc::clone(&session);
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
                                let _ = read_session.events.send(SessionEvent::Output(data));
                            }
                            Err(error) => {
                                warn!(session_id, %error, "PTY read failed");
                                break;
                            }
                        }
                    }
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

            let wait_session = Arc::clone(&session);
            thread::Builder::new()
                .name(format!("forge-pty-wait-{session_id}"))
                .spawn(move || {
                    let status = wait_session
                        .child
                        .lock()
                        .expect("child mutex poisoned")
                        .wait()
                        .ok();
                    let code = status.and_then(|value| value.exit_code());
                    let _ = wait_session.events.send(SessionEvent::Exited(code));
                })
                .context("spawn PTY wait thread")?;

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

    fn append_bounded(backlog: &Mutex<VecDeque<u8>>, data: &[u8]) {
        let mut backlog = backlog.lock().expect("backlog mutex poisoned");
        let overflow = backlog
            .len()
            .saturating_add(data.len())
            .saturating_sub(BACKLOG_LIMIT);
        backlog.drain(..overflow.min(backlog.len()));
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
        let socket = parse_socket()?;
        if socket.exists() {
            std::fs::remove_file(&socket).context("remove stale socket")?;
        }
        let listener = UnixListener::bind(&socket)
            .with_context(|| format!("bind {}", socket.display()))?;
        info!(path = %socket.display(), "terminal daemon listening");
        let daemon = Arc::new(Daemon::default());
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

    fn parse_socket() -> Result<PathBuf> {
        let mut args = std::env::args().skip(1);
        match (args.next().as_deref(), args.next()) {
            (Some("--socket"), Some(path)) => Ok(path.into()),
            (None, None) => Ok("/tmp/forge-prototype.sock".into()),
            _ => bail!("usage: proto-termd [--socket PATH]"),
        }
    }

    async fn handle_connection(stream: UnixStream, daemon: Arc<Daemon>) -> Result<()> {
        let (mut reader, mut writer) = stream.into_split();
        let (out_tx, mut out_rx) = async_mpsc::channel::<(FrameKind, ServerMessage)>(CONNECTION_QUEUE);
        let writer_task = tokio::spawn(async move {
            while let Some((kind, message)) = out_rx.recv().await {
                write_message(&mut writer, kind, &message).await?;
            }
            Ok::<_, proto_ipc::ProtocolError>(())
        });

        let (_, first) = read_message::<_, ClientMessage>(&mut reader).await?;
        match first {
            ClientMessage::Initialize {
                protocol_version,
                ..
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
                out_tx
                    .send((
                        FrameKind::Response,
                        ServerMessage::Attached {
                            session_id,
                            backlog: session.snapshot(),
                        },
                    ))
                    .await?;
                let mut events = session.events.subscribe();
                let forwarding = out_tx.clone();
                tokio::spawn(async move {
                    loop {
                        let message = match events.recv().await {
                            Ok(SessionEvent::Output(data)) => ServerMessage::Output { session_id, data },
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

    async fn send_error(
        sender: &async_mpsc::Sender<(FrameKind, ServerMessage)>,
        message: String,
    ) {
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

