use anyhow::{Context, Result, bail};
use forge_bench::{BenchmarkResult, Thresholds, summarize};
use proto_ipc::{ClientMessage, FrameKind, read_message, write_message};
use serde::Deserialize;
use std::{
    path::{Path, PathBuf},
    process::Command,
    time::Instant,
};

#[derive(Deserialize)]
struct GuiMetrics {
    elapsed_ms: f64,
    samples_ms: Option<Vec<f64>>,
    frames: u64,
    pss_kib: Option<u64>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let scenario = args.next().unwrap_or_else(|| "ipc_round_trip".into());
    let arguments = args.collect::<Vec<_>>();
    let options = parse_options(&arguments)?;
    let result = match scenario.as_str() {
        "ipc_round_trip" => ipc_round_trip(options.iterations).await?,
        "startup_empty" => gui_scenario(
            "startup_empty",
            options.iterations,
            &["--exit-after-first-frame"],
        )?,
        "grid_full" => gui_scenario(
            "grid_full",
            1,
            &["--benchmark-grid-frames", &options.iterations.to_string()],
        )?,
        "panes_20" => gui_scenario(
            "panes_20",
            1,
            &[
                "--benchmark-panes",
                "20",
                "--benchmark-frames",
                &options.iterations.to_string(),
            ],
        )?,
        "idle" => gui_scenario("idle", 1, &["--benchmark-idle-ms", "60000"])?,
        "key_echo" => key_echo::run(options.iterations).await?,
        "flood_input" => key_echo::run_flood(options.iterations).await?,
        "termd_idle" => termd_idle::run(options.iterations).await?,
        _ => bail!(
            "unknown scenario {scenario}; use ipc_round_trip, startup_empty, idle, grid_full, panes_20, key_echo, flood_input, or termd_idle"
        ),
    };
    println!("{}", serde_json::to_string_pretty(&result)?);
    if let Some(path) = options.output.as_deref() {
        save_result(path, &result)?;
    }
    if options.check {
        check_thresholds(&result, options.thresholds.as_deref())?;
    }
    Ok(())
}

#[derive(Debug, PartialEq)]
struct Options {
    iterations: usize,
    check: bool,
    output: Option<PathBuf>,
    /// Budget file for `--check`; defaults to `bench/thresholds.toml`.
    thresholds: Option<PathBuf>,
}

fn save_result(path: &std::path::Path, result: &BenchmarkResult) -> Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .context("benchmark output needs a parent directory")?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("create benchmark directory {}", parent.display()))?;
    let mut encoded = serde_json::to_vec_pretty(result)?;
    encoded.push(b'\n');
    std::fs::write(path, encoded)
        .with_context(|| format!("write benchmark result {}", path.display()))
}

async fn ipc_round_trip(iterations: usize) -> Result<BenchmarkResult> {
    let (mut client, mut server) = tokio::io::duplex(64 * 1024);
    let echo = tokio::spawn(async move {
        for _ in 0..iterations {
            let (kind, message) = read_message::<_, ClientMessage>(&mut server).await?;
            write_message(&mut server, kind, &message).await?;
        }
        Ok::<_, proto_ipc::ProtocolError>(())
    });
    let message = ClientMessage::Input {
        session_id: 1,
        data: vec![b'x'; 32],
    };
    let mut samples = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let started = Instant::now();
        write_message(&mut client, FrameKind::Request, &message).await?;
        let _ = read_message::<_, ClientMessage>(&mut client).await?;
        samples.push(started.elapsed().as_secs_f64() * 1_000.0);
    }
    echo.await??;
    Ok(summarize("ipc_round_trip", samples, &[], None))
}

fn gui_scenario(scenario: &str, iterations: usize, args: &[&str]) -> Result<BenchmarkResult> {
    let executable = forge_gui_executable();
    if !executable.is_file() {
        bail!("{} not found; run cargo build first", executable.display());
    }
    let mut samples = Vec::with_capacity(iterations);
    let mut pss = Vec::new();
    let mut frames = None;
    for _ in 0..iterations {
        let output = Command::new(&executable)
            .args(args)
            .output()
            .with_context(|| format!("run {}", executable.display()))?;
        if !output.status.success() {
            bail!(
                "forge-gui benchmark failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let stdout =
            String::from_utf8(output.stdout).context("forge-gui emitted non-UTF-8 metrics")?;
        let metrics: GuiMetrics = stdout
            .lines()
            .rev()
            .find_map(|line| serde_json::from_str(line).ok())
            .context("forge-gui emitted no JSON metrics")?;
        if let Some(frame_samples) = metrics.samples_ms {
            samples.extend(frame_samples);
        } else {
            samples.push(metrics.elapsed_ms);
        }
        pss.extend(metrics.pss_kib);
        frames = Some(metrics.frames);
    }
    Ok(summarize(scenario, samples, &pss, frames))
}

fn forge_gui_executable() -> PathBuf {
    profile_directory().join("forge-gui")
}

/// Directory of the binaries built in the same profile as this runner.
fn profile_directory() -> PathBuf {
    let executable = std::env::current_exe().expect("resolve forge-bench executable");
    let directory = executable
        .parent()
        .expect("forge-bench executable directory");
    if directory.ends_with("deps") {
        directory.parent().expect("Cargo profile directory").into()
    } else {
        directory.into()
    }
}

/// Bet B from ARCHITECTURE.md: key press → the daemon's screen patch with
/// the echoed character, across the socket, the PTY and Ghostty. The shell
/// is `cat`, so the echo comes from the kernel's line discipline.
#[cfg(unix)]
mod key_echo {
    use super::{BenchmarkResult, profile_directory, summarize};
    use anyhow::{Context, Result, bail};
    use proto_ipc::{
        ClientMessage, FrameKind, FrameReader, KeyAction, KeyEvent, KeyMods, PROTOCOL_VERSION,
        ServerMessage, TerminalKey, write_message,
    };
    use std::{
        path::PathBuf,
        process::Stdio,
        time::{Duration, Instant},
    };
    use tokio::{
        net::{
            UnixStream,
            unix::{OwnedReadHalf, OwnedWriteHalf},
        },
        process::{Child, Command},
        time::timeout,
    };

    struct Session {
        daemon: Child,
        socket: PathBuf,
        reader: FrameReader<OwnedReadHalf>,
        writer: OwnedWriteHalf,
        id: u64,
    }

    /// Starts a private daemon and attaches to the requested command.
    async fn start(command: &str, args: Vec<String>) -> Result<Session> {
        let ghostty = std::env::var_os("FORGE_GHOSTTY_LIB").map_or_else(
            || {
                PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("../../target/ghostty/lib/libghostty-vt.so")
            },
            PathBuf::from,
        );
        if !ghostty.is_file() {
            bail!(
                "{} not found; run scripts/bootstrap-ghostty.sh",
                ghostty.display()
            );
        }
        let socket = std::env::temp_dir().join(format!("forge-bench-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&socket);
        let daemon = Command::new(profile_directory().join("proto-termd"))
            .arg("--socket")
            .arg(&socket)
            .arg("--ghostty-lib")
            .arg(&ghostty)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .context("spawn proto-termd")?;
        let stream = timeout(Duration::from_secs(5), async {
            loop {
                match UnixStream::connect(&socket).await {
                    Ok(stream) => return stream,
                    Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
                }
            }
        })
        .await
        .context("daemon did not open its socket")?;
        let (reader, mut writer) = stream.into_split();
        let mut reader = FrameReader::new(reader);
        write_message(
            &mut writer,
            FrameKind::Request,
            &ClientMessage::Initialize {
                protocol_version: PROTOCOL_VERSION,
                client_name: "forge-bench".into(),
            },
        )
        .await?;
        reader.read_message::<ServerMessage>().await?;
        write_message(
            &mut writer,
            FrameKind::Request,
            &ClientMessage::CreateSession {
                request_id: 1,
                command: command.into(),
                args,
                cwd: std::env::current_dir()?,
                cols: 80,
                rows: 24,
                env: Vec::new(),
            },
        )
        .await?;
        let session_id = match reader.read_message::<ServerMessage>().await?.1 {
            ServerMessage::SessionCreated { session_id, .. } => session_id,
            other => bail!("unexpected response {other:?}"),
        };
        write_message(
            &mut writer,
            FrameKind::Request,
            &ClientMessage::Attach { session_id },
        )
        .await?;
        Ok(Session {
            daemon,
            socket,
            reader,
            writer,
            id: session_id,
        })
    }

    fn key(session_id: u64, key: TerminalKey, text: Option<&str>) -> ClientMessage {
        ClientMessage::Key {
            session_id,
            event: KeyEvent {
                action: KeyAction::Press,
                key,
                mods: KeyMods::default(),
                text: text.map(str::to_string),
                unshifted_codepoint: text
                    .and_then(|text| text.chars().next())
                    .map_or(0, u32::from),
            },
        }
    }

    /// One key press until the screen patch that shows its echo.
    async fn press_and_wait(
        session: &mut Session,
        key_code: TerminalKey,
        text: &str,
    ) -> Result<f64> {
        let started = Instant::now();
        write_message(
            &mut session.writer,
            FrameKind::Notification,
            &key(session.id, key_code, Some(text)),
        )
        .await?;
        let reader = &mut session.reader;
        timeout(Duration::from_secs(2), async {
            loop {
                match reader.read_message::<ServerMessage>().await?.1 {
                    ServerMessage::ScreenPatch { dirty_rows, .. }
                        if dirty_rows
                            .iter()
                            .any(|row| row.cells.iter().any(|cell| cell.text == text)) =>
                    {
                        return Ok::<_, anyhow::Error>(started.elapsed().as_secs_f64() * 1_000.0);
                    }
                    ServerMessage::Error { message } => bail!("daemon: {message}"),
                    _ => {}
                }
            }
        })
        .await
        .context("echo timed out")?
    }

    pub async fn run(iterations: usize) -> Result<BenchmarkResult> {
        let mut session = start("/bin/cat", Vec::new()).await?;
        let mut samples = Vec::with_capacity(iterations);
        for iteration in 0..iterations + 5 {
            // Alternate two characters so consecutive patches always differ.
            let (key_code, text) = if iteration % 2 == 0 {
                (TerminalKey::X, "x")
            } else {
                (TerminalKey::Y, "y")
            };
            let sample = press_and_wait(&mut session, key_code, text).await?;
            // Clear the line so the next character is the only change.
            write_message(
                &mut session.writer,
                FrameKind::Notification,
                &key(session.id, TerminalKey::Backspace, None),
            )
            .await?;
            if iteration >= 5 {
                samples.push(sample);
            }
        }
        session.daemon.kill().await?;
        let _ = std::fs::remove_file(&session.socket);
        Ok(summarize("key_echo", samples, &[], None))
    }

    /// Measures input delivery while a 1 GiB producer is saturating the PTY.
    /// The producer is stopped only after the byte reaches the foreground
    /// reader, which emits a marker through the same congested output path.
    pub async fn run_flood(iterations: usize) -> Result<BenchmarkResult> {
        const READY: &[u8] = b"FORGE_FLOOD_READY";
        const MARKER: &[u8] = b"FORGE_FLOOD_INPUT_OK";
        let script = concat!(
            "stty raw -echo; ",
            "(dd bs=1 count=1 of=/dev/null 2>/dev/null; ",
            "printf FORGE_FLOOD_INPUT_OK) & ",
            "printf FORGE_FLOOD_READY; ",
            "head -c 1073741824 /dev/zero"
        );
        let mut samples = Vec::with_capacity(iterations);
        for _ in 0..iterations {
            let mut session = start("/bin/sh", vec!["-c".into(), script.into()]).await?;

            // The reader is armed before READY. Observe READY and at least one
            // later output chunk so the clock starts only once the 1 GiB
            // producer is actively applying backpressure.
            let mut startup_tail = Vec::new();
            timeout(Duration::from_secs(5), async {
                let mut ready = false;
                loop {
                    if let ServerMessage::Output { data, .. } =
                        session.reader.read_message::<ServerMessage>().await?.1
                    {
                        if ready && !data.is_empty() {
                            return Ok::<_, anyhow::Error>(());
                        }
                        startup_tail.extend_from_slice(&data);
                        ready = startup_tail
                            .windows(READY.len())
                            .any(|window| window == READY);
                        if startup_tail.len() > READY.len() * 2 {
                            startup_tail.drain(..startup_tail.len() - READY.len());
                        }
                    }
                }
            })
            .await
            .context("the 1 GiB producer emitted no output")??;

            let started = Instant::now();
            write_message(
                &mut session.writer,
                FrameKind::Notification,
                &ClientMessage::Input {
                    session_id: session.id,
                    data: vec![b'x'],
                },
            )
            .await?;
            let mut tail = Vec::new();
            let elapsed = timeout(Duration::from_secs(5), async {
                loop {
                    match session.reader.read_message::<ServerMessage>().await?.1 {
                        ServerMessage::Output { data, .. } => {
                            tail.extend_from_slice(&data);
                            if tail.windows(MARKER.len()).any(|window| window == MARKER) {
                                return Ok::<_, anyhow::Error>(
                                    started.elapsed().as_secs_f64() * 1_000.0,
                                );
                            }
                            if tail.len() > MARKER.len() * 2 {
                                tail.drain(..tail.len() - MARKER.len());
                            }
                        }
                        ServerMessage::Error { message } => bail!("daemon: {message}"),
                        _ => {}
                    }
                }
            })
            .await
            .context("input marker timed out under 1 GiB output pressure")??;
            samples.push(elapsed);
            session.daemon.kill().await?;
            let _ = std::fs::remove_file(&session.socket);
        }
        Ok(summarize("flood_input", samples, &[], None))
    }
}

#[cfg(not(unix))]
mod key_echo {
    use super::BenchmarkResult;
    use anyhow::{Result, bail};

    pub async fn run(_iterations: usize) -> Result<BenchmarkResult> {
        bail!("key_echo needs the Unix daemon")
    }

    pub async fn run_flood(_iterations: usize) -> Result<BenchmarkResult> {
        bail!("flood_input needs the Unix daemon")
    }
}

/// PSS of a private daemon after creating N idle PTYs. Child shell processes
/// are intentionally excluded so this measures Forge's per-session overhead.
#[cfg(unix)]
mod termd_idle {
    use super::{BenchmarkResult, profile_directory, summarize};
    use anyhow::{Context, Result, bail};
    use proto_ipc::{
        ClientMessage, FrameKind, FrameReader, PROTOCOL_VERSION, ServerMessage, write_message,
    };
    use std::{path::PathBuf, process::Stdio, time::Duration};
    use tokio::{net::UnixStream, process::Command, time::timeout};

    async fn connect(socket: &PathBuf) -> Result<UnixStream> {
        timeout(Duration::from_secs(5), async {
            loop {
                match UnixStream::connect(socket).await {
                    Ok(stream) => return stream,
                    Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
                }
            }
        })
        .await
        .context("daemon did not open its socket")
    }

    fn process_pss_kib(pid: u32) -> Result<u64> {
        let path = format!("/proc/{pid}/smaps_rollup");
        let contents = std::fs::read_to_string(&path)
            .with_context(|| format!("read daemon memory from {path}"))?;
        contents
            .lines()
            .find_map(|line| line.strip_prefix("Pss:"))
            .and_then(|value| value.split_whitespace().next())
            .context("smaps_rollup contains no Pss")?
            .parse()
            .context("invalid Pss value")
    }

    pub async fn run(session_count: usize) -> Result<BenchmarkResult> {
        let ghostty = std::env::var_os("FORGE_GHOSTTY_LIB").map_or_else(
            || {
                PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("../../target/ghostty/lib/libghostty-vt.so")
            },
            PathBuf::from,
        );
        if !ghostty.is_file() {
            bail!(
                "{} not found; run scripts/bootstrap-ghostty.sh",
                ghostty.display()
            );
        }
        let executable = profile_directory().join("proto-termd");
        if !executable.is_file() {
            bail!(
                "{} not found; build proto-termd first",
                executable.display()
            );
        }
        let socket = std::env::temp_dir().join(format!(
            "forge-bench-termd-idle-{}.sock",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&socket);
        let mut daemon = Command::new(&executable)
            .arg("--socket")
            .arg(&socket)
            .arg("--ghostty-lib")
            .arg(&ghostty)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("spawn {}", executable.display()))?;
        let pid = daemon.id().context("daemon exited before measurement")?;
        let stream = connect(&socket).await?;
        let (reader, mut writer) = stream.into_split();
        let mut reader = FrameReader::new(reader);
        write_message(
            &mut writer,
            FrameKind::Request,
            &ClientMessage::Initialize {
                protocol_version: PROTOCOL_VERSION,
                client_name: "forge-bench".into(),
            },
        )
        .await?;
        reader.read_message::<ServerMessage>().await?;

        for request_id in 1..=session_count as u64 {
            write_message(
                &mut writer,
                FrameKind::Request,
                &ClientMessage::CreateSession {
                    request_id,
                    command: "/bin/sh".into(),
                    args: vec!["-c".into(), "sleep 600".into()],
                    cwd: std::env::current_dir()?,
                    cols: 80,
                    rows: 24,
                    env: Vec::new(),
                },
            )
            .await?;
            match reader.read_message::<ServerMessage>().await?.1 {
                ServerMessage::SessionCreated { .. } => {}
                other => bail!("unexpected create-session response {other:?}"),
            }
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
        let pss = process_pss_kib(pid)?;
        daemon.kill().await?;
        let _ = std::fs::remove_file(&socket);
        Ok(summarize(
            "termd_idle",
            vec![0.0; session_count],
            &[pss],
            None,
        ))
    }
}

#[cfg(not(unix))]
mod termd_idle {
    use super::BenchmarkResult;
    use anyhow::{Result, bail};

    pub async fn run(_session_count: usize) -> Result<BenchmarkResult> {
        bail!("termd_idle needs the Unix daemon")
    }
}

fn check_thresholds(result: &BenchmarkResult, thresholds: Option<&Path>) -> Result<()> {
    let path = thresholds.map_or_else(
        || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../bench/thresholds.toml"),
        Path::to_path_buf,
    );
    let contents = std::fs::read_to_string(&path)
        .with_context(|| format!("read thresholds from {}", path.display()))?;
    let thresholds: Thresholds = toml::from_str(&contents).context("parse benchmark thresholds")?;
    let violations = thresholds.violations(result);
    if violations.is_empty() {
        println!("thresholds: PASS");
        Ok(())
    } else {
        bail!("thresholds: FAIL: {}", violations.join("; "))
    }
}

fn parse_options(args: &[String]) -> Result<Options> {
    let mut options = Options {
        iterations: 10,
        check: false,
        output: None,
        thresholds: None,
    };
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--check" => options.check = true,
            "--thresholds" => {
                index += 1;
                let value = args.get(index).context("--thresholds requires a path")?;
                options.thresholds = Some(PathBuf::from(value));
            }
            "--output" => {
                index += 1;
                options.output = Some(args.get(index).context("--output requires a path")?.into());
            }
            "--iterations" => {
                index += 1;
                let value = args.get(index).context("--iterations requires a value")?;
                options.iterations = value.parse().context("invalid iteration count")?;
                if options.iterations == 0 {
                    bail!("iterations must be greater than zero");
                }
            }
            argument => bail!("unknown argument {argument}"),
        }
        index += 1;
    }
    Ok(options)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_iteration_arguments() {
        assert_eq!(
            parse_options(&[]).unwrap(),
            Options {
                iterations: 10,
                check: false,
                output: None,
                thresholds: None,
            }
        );
        assert_eq!(
            parse_options(&[
                "--check".into(),
                "--iterations".into(),
                "3".into(),
                "--thresholds".into(),
                "bench/thresholds.ci.toml".into(),
            ])
            .unwrap(),
            Options {
                iterations: 3,
                check: true,
                output: None,
                thresholds: Some(PathBuf::from("bench/thresholds.ci.toml")),
            }
        );
        assert!(parse_options(&["--iterations".into(), "0".into()]).is_err());
        assert_eq!(
            parse_options(&["--output".into(), "bench/results/example.json".into()])
                .unwrap()
                .output,
            Some(PathBuf::from("bench/results/example.json"))
        );
    }

    #[test]
    fn saves_pretty_json_result() {
        let directory =
            std::env::temp_dir().join(format!("forge-bench-test-{}", std::process::id()));
        let path = directory.join("nested/result.json");
        let result = summarize("ipc_round_trip", vec![0.25], &[], None);
        save_result(&path, &result).unwrap();
        assert_eq!(
            serde_json::from_slice::<BenchmarkResult>(&std::fs::read(path).unwrap()).unwrap(),
            result
        );
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn resolves_gui_in_workspace_target() {
        assert!(forge_gui_executable().ends_with("debug/forge-gui"));
    }
}
