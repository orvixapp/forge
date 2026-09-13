# Forge prototype

This repository is implementing the Phase 0 prototype described in
[`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md).

The first vertical slice validates the internal IPC boundary and a persistent
PTY daemon before adding the GPU renderer.

## Run

```bash
FORGE_ZIG=/path/to/zig ./scripts/bootstrap-ghostty.sh
cargo test --workspace
cargo run
cargo run --release -p forge-bench -- ipc_round_trip --iterations 10000 --check
cargo run --release -p forge-bench -- startup_empty --iterations 10 --check
cargo run --release -p forge-bench -- idle --check
cargo run --release -p forge-bench -- grid_full --iterations 120 --check
```

`cargo run` opens the GPUI client. It connects to an existing terminal daemon
or starts one automatically, creates a shell, forwards keyboard input, and
stops the daemon it owns when the window closes. Building Ghostty is a one-time
bootstrap step.

The benchmark runner emits JSON. With `--check`, it also enforces the versioned
budgets in `bench/thresholds.toml`; the idle scenario runs for 60 seconds. Build
`forge-gui` in the same profile before running the GUI scenarios.

## Current scope

- Implemented: versioned MessagePack framing, bounded frames, PTY lifecycle,
  attach/detach, bounded raw backlog, terminal resize/input, Ghostty VT parsing
  and incremental styled screen cells, GPUI terminal grid and keyboard input,
  rope edits and undo, JSON-RPC/JSONL transport primitives for the ACP spike,
  benchmark harness for IPC latency, cold startup, PSS, and idle redraws.
- Next: ACP capability flow, benchmark history persistence and the Open VSX API
  scanner.
