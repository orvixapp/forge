#![cfg(unix)]

use proto_ipc::{
    ClientMessage, FrameKind, PROTOCOL_VERSION, ServerMessage, read_message, write_message,
};
use std::{path::PathBuf, process::Stdio, time::Duration};
use tokio::{net::UnixStream, process::Command, time::timeout};

#[tokio::test]
async fn command_output_crosses_the_daemon_boundary() {
    let socket = unique_socket();
    let mut daemon = Command::new(env!("CARGO_BIN_EXE_proto-termd"))
        .arg("--socket")
        .arg(&socket)
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
            args: vec!["-lc".into(), "printf forge-terminal-ok".into()],
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

    let output = timeout(Duration::from_secs(5), async {
        let mut output = Vec::new();
        loop {
            match read_message::<_, ServerMessage>(&mut reader)
                .await
                .unwrap()
                .1
            {
                ServerMessage::Attached { backlog, .. } => output.extend(backlog),
                ServerMessage::Output { data, .. } => output.extend(data),
                ServerMessage::Exited { exit_code, .. } => {
                    assert_eq!(exit_code, Some(0));
                    return output;
                }
                ServerMessage::Error { message } => panic!("daemon error: {message}"),
                _ => {}
            }
        }
    })
    .await
    .expect("terminal command timed out");

    assert_eq!(output, b"forge-terminal-ok");
    daemon.kill().await.expect("stop daemon");
    let _ = std::fs::remove_file(socket);
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
