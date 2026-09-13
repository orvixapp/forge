#[cfg(not(unix))]
fn main() {
    eprintln!("proto-term-client currently implements Unix sockets only");
    std::process::exit(2);
}

#[cfg(unix)]
mod unix {
    use anyhow::{Context, Result, bail};
    use proto_ipc::{
        ClientMessage, FrameKind, PROTOCOL_VERSION, ServerMessage, read_message, write_message,
    };
    use std::path::PathBuf;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::UnixStream,
    };

    struct Options {
        socket: PathBuf,
        attach: Option<u64>,
        command: Vec<String>,
    }

    pub async fn run() -> Result<()> {
        let options = parse_args()?;
        let stream = UnixStream::connect(&options.socket)
            .await
            .with_context(|| format!("connect to {}", options.socket.display()))?;
        let (mut reader, mut writer) = stream.into_split();
        write_message(
            &mut writer,
            FrameKind::Request,
            &ClientMessage::Initialize {
                protocol_version: PROTOCOL_VERSION,
                client_name: "proto-term-client".into(),
            },
        )
        .await?;
        expect_initialized(&mut reader).await?;

        let session_id = if let Some(id) = options.attach {
            id
        } else {
            let command = options
                .command
                .first()
                .cloned()
                .unwrap_or_else(default_shell);
            let args = options.command.into_iter().skip(1).collect();
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
            match read_message::<_, ServerMessage>(&mut reader).await?.1 {
                ServerMessage::SessionCreated { session_id, .. } => {
                    eprintln!("Forge session: {session_id}");
                    session_id
                }
                other => bail!("expected session_created, received {other:?}"),
            }
        };

        write_message(
            &mut writer,
            FrameKind::Request,
            &ClientMessage::Attach { session_id },
        )
        .await?;
        let mut stdout = tokio::io::stdout();
        match read_message::<_, ServerMessage>(&mut reader).await?.1 {
            ServerMessage::Attached { backlog, .. } => stdout.write_all(&backlog).await?,
            other => bail!("expected attached, received {other:?}"),
        }
        stdout.flush().await?;

        let input_task = tokio::spawn(async move {
            let mut stdin = tokio::io::stdin();
            let mut chunk = vec![0_u8; 4096];
            loop {
                let count = stdin.read(&mut chunk).await?;
                if count == 0 {
                    break;
                }
                write_message(
                    &mut writer,
                    FrameKind::Notification,
                    &ClientMessage::Input {
                        session_id,
                        data: chunk[..count].to_vec(),
                    },
                )
                .await?;
            }
            Ok::<_, proto_ipc::ProtocolError>(())
        });

        loop {
            match read_message::<_, ServerMessage>(&mut reader).await {
                Ok((_, ServerMessage::Output { data, .. })) => {
                    stdout.write_all(&data).await?;
                    stdout.flush().await?;
                }
                Ok((_, ServerMessage::Exited { exit_code, .. })) => {
                    eprintln!("\nForge session exited: {exit_code:?}");
                    input_task.abort();
                    return Ok(());
                }
                Ok((_, ServerMessage::Error { message })) => bail!("daemon: {message}"),
                Ok(_) => {}
                Err(error) => {
                    input_task.abort();
                    return Err(error.into());
                }
            }
        }
    }

    async fn expect_initialized<R: tokio::io::AsyncRead + Unpin>(reader: &mut R) -> Result<()> {
        match read_message::<_, ServerMessage>(reader).await?.1 {
            ServerMessage::Initialized { protocol_version }
                if protocol_version == PROTOCOL_VERSION =>
            {
                Ok(())
            }
            other => bail!("daemon initialization failed: {other:?}"),
        }
    }

    fn default_shell() -> String {
        std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into())
    }

    fn parse_args() -> Result<Options> {
        let mut args = std::env::args().skip(1).peekable();
        let mut socket = PathBuf::from("/tmp/forge-prototype.sock");
        let mut attach = None;
        while let Some(arg) = args.peek() {
            match arg.as_str() {
                "--socket" => {
                    args.next();
                    socket = args.next().context("--socket requires a path")?.into();
                }
                "--attach" => {
                    args.next();
                    attach = Some(
                        args.next()
                            .context("--attach requires a session id")?
                            .parse()
                            .context("invalid session id")?,
                    );
                }
                "--" => {
                    args.next();
                    break;
                }
                value if value.starts_with('-') => bail!("unknown option {value}"),
                _ => break,
            }
        }
        Ok(Options {
            socket,
            attach,
            command: args.collect(),
        })
    }
}

#[cfg(unix)]
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    unix::run().await
}
