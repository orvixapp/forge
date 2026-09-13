#![cfg(unix)]

use proto_ipc::{
    ClientMessage, FrameKind, PROTOCOL_VERSION, ServerMessage, read_message, write_message,
};
use std::{path::PathBuf, process::Stdio, time::Duration};
use tokio::{
    net::{UnixStream, unix::OwnedReadHalf},
    process::Command,
    time::timeout,
};

#[tokio::test]
async fn command_output_crosses_the_daemon_boundary() {
    let Some(ghostty_lib) = ghostty_library() else {
        eprintln!("skipping Ghostty integration test; set FORGE_GHOSTTY_LIB");
        return;
    };
    let socket = unique_socket();
    let mut daemon = Command::new(env!("CARGO_BIN_EXE_proto-termd"))
        .arg("--socket")
        .arg(&socket)
        .arg("--ghostty-lib")
        .arg(ghostty_lib)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn daemon");

    let stream = timeout(Duration::from_secs(5), connect_when_ready(&socket))
        .await
        .expect("daemon startup timed out")
        .expect("connect to daemon");
    let (mut reader, mut writer) = stream.into_split();

    write_message(
        &mut writer,
        FrameKind::Request,
        &ClientMessage::Initialize {
            protocol_version: PROTOCOL_VERSION,
            client_name: "integration-test".into(),
        },
    )
    .await
    .unwrap();
    assert!(matches!(
        read_message::<_, ServerMessage>(&mut reader)
            .await
            .unwrap()
            .1,
        ServerMessage::Initialized { .. }
    ));

    write_message(
        &mut writer,
        FrameKind::Request,
        &ClientMessage::CreateSession {
            request_id: 9,
            command: "/bin/sh".into(),
            args: vec![
                "-lc".into(),
                "printf '\\033[31mforge-terminal-ok\\033[0m'".into(),
            ],
            cols: 80,
            rows: 24,
        },
    )
    .await
    .unwrap();
    let session_id = match read_message::<_, ServerMessage>(&mut reader)
        .await
        .unwrap()
        .1
    {
        ServerMessage::SessionCreated { session_id, .. } => session_id,
        other => panic!("unexpected response: {other:?}"),
    };
    write_message(
        &mut writer,
        FrameKind::Request,
        &ClientMessage::Attach { session_id },
    )
    .await
    .unwrap();

    let (output, screen, screen_meta, colored) =
        timeout(Duration::from_secs(5), collect_output(&mut reader))
            .await
            .expect("terminal command timed out");

    assert_eq!(output, b"\x1b[31mforge-terminal-ok\x1b[0m");
    assert_eq!(screen, "forge-terminal-ok");
    let (revision, cols, rows) = screen_meta.expect("Ghostty render frame");
    assert!(revision > 0);
    assert_eq!((cols, rows), (80, 24));
    assert!(colored, "ANSI foreground color was not resolved by Ghostty");
    daemon.kill().await.expect("stop daemon");
    let _ = std::fs::remove_file(socket);
}

async fn collect_output(
    reader: &mut OwnedReadHalf,
) -> (Vec<u8>, String, Option<(u64, u16, u16)>, bool) {
    let mut output = Vec::new();
    let mut screen = String::new();
    let mut screen_meta = None;
    let mut colored = false;
    loop {
        match read_message::<_, ServerMessage>(reader).await.unwrap().1 {
            ServerMessage::Attached { backlog, .. } => output.extend(backlog),
            ServerMessage::Output { data, .. } => output.extend(data),
            ServerMessage::ScreenPatch {
                revision,
                cols,
                rows,
                dirty_rows,
                ..
            } => {
                colored |= dirty_rows
                    .iter()
                    .flat_map(|row| &row.cells)
                    .any(|cell| !cell.text.is_empty() && cell.foreground.is_some());
                screen = dirty_rows
                    .into_iter()
                    .flat_map(|row| row.cells)
                    .map(|cell| cell.text)
                    .collect();
                screen_meta = Some((revision, cols, rows));
            }
            ServerMessage::Exited { exit_code, .. } => {
                assert_eq!(exit_code, Some(0));
                return (output, screen, screen_meta, colored);
            }
            ServerMessage::Error { message } => panic!("daemon error: {message}"),
            _ => {}
        }
    }
}

fn ghostty_library() -> Option<PathBuf> {
    std::env::var_os("FORGE_GHOSTTY_LIB")
        .map(PathBuf::from)
        .filter(|path| path.is_file())
}

async fn connect_when_ready(socket: &PathBuf) -> std::io::Result<UnixStream> {
    loop {
        match UnixStream::connect(socket).await {
            Ok(stream) => return Ok(stream),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Err(error) => return Err(error),
        }
    }
}

fn unique_socket() -> PathBuf {
    std::env::temp_dir().join(format!("forge-termd-test-{}.sock", std::process::id()))
}
