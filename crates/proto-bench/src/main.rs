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
        _ => bail!(
            "unknown scenario {scenario}; use ipc_round_trip, startup_empty, idle, grid_full, or panes_20"
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
