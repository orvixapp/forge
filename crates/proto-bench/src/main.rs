use anyhow::{Context, Result, bail};
use forge_bench::{BenchmarkResult, Thresholds, summarize};
use proto_ipc::{ClientMessage, FrameKind, read_message, write_message};
use serde::Deserialize;
use std::{path::PathBuf, process::Command, time::Instant};

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
        "idle" => gui_scenario("idle", 1, &["--benchmark-idle-ms", "60000"])?,
        _ => bail!(
            "unknown scenario {scenario}; use ipc_round_trip, startup_empty, idle, or grid_full"
        ),
    };
    println!("{}", serde_json::to_string_pretty(&result)?);
    if options.check {
        check_thresholds(&result)?;
    }
    Ok(())
}

#[derive(Debug, PartialEq)]
struct Options {
    iterations: usize,
    check: bool,
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
    let executable = std::env::current_exe().expect("resolve forge-bench executable");
    let directory = executable
        .parent()
        .expect("forge-bench executable directory");
    let profile_directory = if directory.ends_with("deps") {
        directory.parent().expect("Cargo profile directory")
    } else {
        directory
    };
    profile_directory.join("forge-gui")
}

fn check_thresholds(result: &BenchmarkResult) -> Result<()> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../bench/thresholds.toml");
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
    };
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--check" => options.check = true,
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
                check: false
            }
        );
        assert_eq!(
            parse_options(&["--check".into(), "--iterations".into(), "3".into()]).unwrap(),
            Options {
                iterations: 3,
                check: true
            }
        );
        assert!(parse_options(&["--iterations".into(), "0".into()]).is_err());
    }

    #[test]
    fn resolves_gui_in_workspace_target() {
        assert!(forge_gui_executable().ends_with("debug/forge-gui"));
    }
}
