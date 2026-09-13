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
cargo run -p proto-bench -- --iterations 10000
```

`cargo run` opens the GPUI client. It connects to an existing terminal daemon
or starts one automatically, creates a shell, forwards keyboard input, and
stops the daemon it owns when the window closes. Building Ghostty is a one-time
bootstrap step.

## Current scope

- Implemented: versioned MessagePack framing, bounded frames, PTY lifecycle,
  attach/detach, bounded raw backlog, terminal resize/input, Ghostty VT parsing
  and incremental styled screen cells, GPUI terminal grid and keyboard input,
  rope edits and undo, JSON-RPC/JSONL transport primitives for the ACP spike,
  IPC benchmark.
- Next: per-cell GPU styling and cursor, resize propagation, ACP capability
  flow, benchmark result persistence and the Open VSX API scanner.
