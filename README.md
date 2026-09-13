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
`forge-gui` in the same profile before running the GUI scenarios, and run them
one at a time: a second window opening or closing on top counts as focus
changes and redraws. `grid_full` measures state update → `present()` per frame
and reports `painted_cells` so culling cannot hide work; set
`ZED_MEASUREMENTS=1` to have GPUI print its own draw+present time per frame.

## Configuration

`forge-gui` reads `~/.config/forge/config.toml` (`$XDG_CONFIG_HOME`,
`$FORGE_CONFIG` or `--config <path>` override it). Every key is optional:

```toml
[font]
family = "monospace"   # generic names resolve to an installed monospace family
size = 14              # pixels
line_height = 1.3      # cell height as a multiple of the size

[colors]
background = "#111318"
foreground = "#d8dee9"
cursor = "#ebcb8b"
selection = "#88c0d0"
selection_opacity = 0.35
accent = "#88c0d0"     # status line

[terminal]
shell = "/bin/zsh"     # default: $SHELL
args = []
padding = 16
show_status = true
```

Mouse: drag to select, double-click selects a word, triple-click a line; a
finished selection lands on the Linux primary buffer (middle click).
Keys: `Ctrl+Shift+C` copies, `Ctrl+Shift+V` pastes the clipboard,
`Shift+Insert` pastes the primary buffer. Typing clears the selection. The
window closes when the shell exits.

## Current scope

- Implemented: versioned MessagePack framing, bounded frames, PTY lifecycle,
  attach/detach, bounded raw backlog, terminal resize/input, Ghostty VT parsing
  and incremental styled screen cells, a direct-paint GPUI grid element (one
  quad per background run, one sprite per glyph, glyph cache, block/bar/
  underline cursors), mouse selection with clipboard and primary-buffer
  copy/paste, xterm key encoding with modifiers, a TOML config for fonts,
  colours and shell, rope edits and undo, JSON-RPC/JSONL
  transport primitives for the ACP spike, benchmark harness for IPC latency,
  cold startup, PSS, idle redraws and the fully dirty 200×60 grid.
- Measured: see `bench/results/2026-09-13-grid-full.md` for the grid spike
  numbers and the reading behind bet A.
- Next: incremental terminal redraw (cached unchanged rows), ACP capability
  flow, benchmark history persistence and the Open VSX API scanner.
