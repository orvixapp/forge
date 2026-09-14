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
                "printf '\\033[31mforge-terminal-ok\\033[0m'; sleep 0.2".into(),
            ],
            cwd: std::env::current_dir().unwrap(),
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
    write_message(
        &mut writer,
        FrameKind::Notification,
        &ClientMessage::Resize {
            session_id,
            cols: 100,
            rows: 30,
        },
    )
    .await
    .unwrap();

    let (output, screen, screen_meta, colored, cursor_seen) =
        timeout(Duration::from_secs(5), collect_output(&mut reader))
            .await
            .expect("terminal command timed out");

    assert_eq!(output, b"\x1b[31mforge-terminal-ok\x1b[0m");
    assert_eq!(screen, "forge-terminal-ok");
    let (revision, cols, rows) = screen_meta.expect("Ghostty render frame");
    assert!(revision > 0);
    assert_eq!((cols, rows), (100, 30));
    assert!(colored, "ANSI foreground color was not resolved by Ghostty");
    assert!(cursor_seen, "Ghostty cursor state did not cross IPC");
    daemon.kill().await.expect("stop daemon");
    let _ = std::fs::remove_file(socket);
}

async fn collect_output(
    reader: &mut OwnedReadHalf,
) -> (Vec<u8>, String, Option<(u64, u16, u16)>, bool, bool) {
    let mut output = Vec::new();
    let mut screen = String::new();
    let mut screen_meta = None;
    let mut colored = false;
    let mut cursor_seen = false;
    loop {
        match read_message::<_, ServerMessage>(reader).await.unwrap().1 {
            ServerMessage::Attached { backlog, .. } => output.extend(backlog),
            ServerMessage::Output { data, .. } => output.extend(data),
            ServerMessage::ScreenPatch {
                revision,
                cols,
                rows,
                dirty_rows,
                cursor,
                ..
            } => {
                cursor_seen |= cursor.is_some_and(|cursor| cursor.visible);
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
                return (output, screen, screen_meta, colored, cursor_seen);
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

/// Scrollback, key encoding, paste and session info through the socket.
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn scrollback_keys_and_session_info_cross_the_daemon_boundary() {
    let Some(ghostty_lib) = ghostty_library() else {
        eprintln!("skipping Ghostty integration test; set FORGE_GHOSTTY_LIB");
        return;
    };
    let socket =
        std::env::temp_dir().join(format!("forge-termd-scroll-{}.sock", std::process::id()));
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
    read_message::<_, ServerMessage>(&mut reader).await.unwrap();
    write_message(
        &mut writer,
        FrameKind::Request,
        &ClientMessage::CreateSession {
            request_id: 1,
            command: "/bin/sh".into(),
            args: vec![
                "-c".into(),
                // 100 lines of scrollback, then mouse tracking + a title, then
                // `cat` keeps the PTY open and echoes what we type.
                "seq 1 100; printf '\\033[?1000h\\033]2;forge-title\\007'; cat".into(),
            ],
            cwd: std::env::current_dir().unwrap(),
            cols: 40,
            rows: 10,
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

    // Once `seq` finished the live screen sits at the bottom of 100+ rows.
    let viewport = wait_for(&mut reader, |message| match message {
        ServerMessage::ScreenPatch { viewport, .. } if viewport.total >= 100 => Some(*viewport),
        _ => None,
    })
    .await;
    assert!(!viewport.scrolled_back(), "{viewport:?}");
    assert_eq!(viewport.len, 10);

    // The application enabled mouse tracking and set a title.
    let info = wait_for(&mut reader, |message| match message {
        ServerMessage::SessionInfo {
            title,
            mouse_tracking,
            ..
        } if *mouse_tracking && title == "forge-title" => Some(()),
        _ => None,
    })
    .await;
    assert_eq!(info, ());

    // Scrolling to the top shows row "1" and reports offset 0.
    write_message(
        &mut writer,
        FrameKind::Notification,
        &ClientMessage::Scroll {
            session_id,
            scroll: proto_ipc::ScrollRequest::Top,
        },
    )
    .await
    .unwrap();
    let top_row = wait_for(&mut reader, |message| match message {
        ServerMessage::ScreenPatch {
            viewport,
            dirty_rows,
            ..
        } if viewport.offset == 0 && viewport.scrolled_back() => {
            dirty_rows.iter().find(|row| row.y == 0).map(|row| {
                row.cells
                    .iter()
                    .map(|cell| cell.text.as_str())
                    .collect::<String>()
            })
        }
        _ => None,
    })
    .await;
    assert!(top_row.starts_with('1'), "top row was {top_row:?}");

    // Pasting jumps back to the live screen; `cat` echoes the text.
    write_message(
        &mut writer,
        FrameKind::Notification,
        &ClientMessage::Paste {
            session_id,
            text: "forge-paste".into(),
        },
    )
    .await
    .unwrap();
    write_message(
        &mut writer,
        FrameKind::Notification,
        &ClientMessage::Key {
            session_id,
            event: proto_ipc::KeyEvent {
                action: proto_ipc::KeyAction::Press,
                key: proto_ipc::TerminalKey::Enter,
                mods: proto_ipc::KeyMods::default(),
                text: None,
                unshifted_codepoint: 0,
            },
        },
    )
    .await
    .unwrap();
    let echoed = wait_for(&mut reader, |message| match message {
        ServerMessage::ScreenPatch {
            viewport,
            dirty_rows,
            ..
        } if !viewport.scrolled_back()
            && dirty_rows.iter().any(|row| {
                row.cells
                    .iter()
                    .map(|cell| cell.text.as_str())
                    .collect::<String>()
                    .contains("forge-paste")
            }) =>
        {
            Some(())
        }
        _ => None,
    })
    .await;
    assert_eq!(echoed, ());

    // A mouse press is encoded for normal tracking as `ESC [ M` + button and
    // 1-based coordinates offset by 32; the tty echoes ESC as `^[`.
    write_message(
        &mut writer,
        FrameKind::Notification,
        &ClientMessage::Mouse {
            session_id,
            event: proto_ipc::MouseEvent {
                action: proto_ipc::MouseAction::Press,
                button: Some(proto_ipc::MouseButton::Left),
                mods: proto_ipc::KeyMods::default(),
                col: 2,
                row: 3,
            },
        },
    )
    .await
    .unwrap();
    let mouse_bytes = wait_for(&mut reader, |message| match message {
        ServerMessage::Output { data, .. } if data.windows(5).any(|window| window == b"[M #$") => {
            Some(data.clone())
        }
        _ => None,
    })
    .await;
    assert!(!mouse_bytes.is_empty());

    // Ctrl+D ends `cat`, so the shell exits and the session reports it. The
    // first one only flushes the pending mouse bytes; the second is EOF.
    for _ in 0..2 {
        write_message(
            &mut writer,
            FrameKind::Notification,
            &ClientMessage::Key {
                session_id,
                event: proto_ipc::KeyEvent {
                    action: proto_ipc::KeyAction::Press,
                    key: proto_ipc::TerminalKey::D,
                    mods: proto_ipc::KeyMods {
                        control: true,
                        ..proto_ipc::KeyMods::default()
                    },
                    text: None,
                    unshifted_codepoint: u32::from('d'),
                },
            },
        )
        .await
        .unwrap();
    }
    let exit = wait_for(&mut reader, |message| match message {
        ServerMessage::Exited { exit_code, .. } => Some(*exit_code),
        _ => None,
    })
    .await;
    assert_eq!(exit, Some(0));
    daemon.kill().await.expect("stop daemon");
    let _ = std::fs::remove_file(socket);
}

/// Reads messages until `pick` accepts one, failing after five seconds.
async fn wait_for<T>(reader: &mut OwnedReadHalf, pick: impl Fn(&ServerMessage) -> Option<T>) -> T {
    timeout(Duration::from_secs(5), async {
        loop {
            let (_, message) = read_message::<_, ServerMessage>(reader).await.unwrap();
            if std::env::var_os("FORGE_TEST_TRACE").is_some() {
                match &message {
                    ServerMessage::ScreenPatch {
                        revision,
                        viewport,
                        dirty_rows,
                        cursor,
                        ..
                    } => eprintln!(
                        "patch rev={revision} viewport={viewport:?} rows={} cursor={cursor:?}",
                        dirty_rows.len()
                    ),
                    ServerMessage::Output { data, .. } => eprintln!("output {data:?}"),
                    other => eprintln!("{other:?}"),
                }
            }
            if let ServerMessage::Error { message } = &message {
                panic!("daemon error: {message}");
            }
            if let Some(value) = pick(&message) {
                return value;
            }
        }
    })
    .await
    .expect("timed out waiting for a daemon message")
}
