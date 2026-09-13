use anyhow::{Context, Result};
use proto_ipc::{ClientMessage, FrameKind, read_message, write_message};
use serde::Serialize;
use std::time::Instant;

#[derive(Serialize)]
struct ResultRow {
    scenario: &'static str,
    iterations: usize,
    median_microseconds: f64,
    p95_microseconds: f64,
}

#[tokio::main]
async fn main() -> Result<()> {
    let iterations = parse_iterations()?;
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
        samples.push(started.elapsed().as_secs_f64() * 1_000_000.0);
    }
    echo.await??;
    samples.sort_by(f64::total_cmp);
    let row = ResultRow {
        scenario: "ipc_messagepack_duplex_round_trip",
        iterations,
        median_microseconds: percentile(&samples, 0.50),
        p95_microseconds: percentile(&samples, 0.95),
    };
    println!("{}", serde_json::to_string_pretty(&row)?);
    Ok(())
}

fn parse_iterations() -> Result<usize> {
    let mut args = std::env::args().skip(1);
    match (args.next().as_deref(), args.next()) {
        (Some("--iterations"), Some(value)) => value.parse().context("invalid iteration count"),
        (None, None) => Ok(10_000),
        _ => anyhow::bail!("usage: proto-bench [--iterations N]"),
    }
}

fn percentile(sorted: &[f64], quantile: f64) -> f64 {
    let index = ((sorted.len() - 1) as f64 * quantile).round() as usize;
    sorted[index]
}

