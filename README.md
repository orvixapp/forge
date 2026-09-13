# Forge prototype

This repository is implementing the Phase 0 prototype described in
[`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md).

The first vertical slice validates the internal IPC boundary and a persistent
PTY daemon before adding the GPU renderer.

## Run

```bash
FORGE_ZIG=/path/to/zig ./scripts/bootstrap-ghostty.sh
cargo test --workspace
FORGE_GHOSTTY_LIB=target/ghostty/lib/libghostty-vt.so \
  cargo run -p proto-termd -- --socket /tmp/forge-prototype.sock
cargo run -p proto-term-client -- --socket /tmp/forge-prototype.sock -- sh -lc 'printf "hello from Forge\\n"'
cargo run -p proto-bench -- --iterations 10000
```

The terminal client creates a PTY session, attaches to it, forwards stdin and
prints output. Disconnecting the client does not terminate the daemon session;
use `--attach SESSION_ID` to reconnect while the daemon is alive.

## Current scope

- Implemented: versioned MessagePack framing, bounded frames, PTY lifecycle,
  attach/detach, bounded raw backlog, terminal resize/input, Ghostty VT parsing
  and plain-text screen snapshots, rope edits and undo, JSON-RPC/JSONL transport
  primitives for the ACP spike, IPC benchmark.
- Next: Ghostty render-state dirty rows, GPUI text-grid spike, ACP capability flow,
  benchmark result persistence and the Open VSX API scanner.
