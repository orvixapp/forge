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
cargo run --release -p forge-bench -- panes_20 --iterations 120 --check
cargo run --release -p forge-bench -- key_echo --iterations 500 --check
cargo run --release -p forge-bench -- flood_input --iterations 50 --check
cargo run --release -p forge-bench -- termd_idle --iterations 200 --check
```

`cargo run` opens the GPUI client. It connects to an existing terminal daemon
or starts one automatically, creates a shell, forwards keyboard input, and
stops the daemon it owns when the window closes. Building Ghostty is a one-time
bootstrap step.

On Linux, install the user-scoped desktop entry once to make the taskbar match
Forge's Wayland/X11 application ID and icon:

```bash
./scripts/install-linux-desktop-integration.sh
```

The benchmark runner emits JSON. With `--check`, it also enforces the versioned
budgets in `bench/thresholds.toml`; the idle scenario runs for 60 seconds. Build
`forge-gui` in the same profile before running the GUI scenarios, and run them
one at a time: a second window opening or closing on top counts as focus
changes and redraws. `grid_full` measures state update → `present()` per frame
and reports `painted_cells` so culling cannot hide work; set
`ZED_MEASUREMENTS=1` to have GPUI print its own draw+present time per frame.

## Configuration

`forge-gui` reads `~/.config/forge/config.toml` (`$XDG_CONFIG_HOME`,
`$FORGE_CONFIG` or `--config <path>` override it), then `<cwd>/.forge/config.toml`
on top (the workspace layer may not set `terminal.shell`/`args`). Changes apply
within a second; an invalid file keeps the running configuration and shows a
notification. Every key is optional; `docs/config.schema.json` (from
`forge-gui --print-config-schema`) gives editors validation and completion:

```toml
[font]
family = "monospace"   # generic names resolve to an installed monospace family
size = 14              # pixels
line_height = 1.3      # cell height as a multiple of the size

[ui]
language = "spanish"   # or "english"
theme = "forge-dark"   # forge-dark, forge-light, or themes/<name>.toml

[colors]               # overrides on top of the theme
cursor = "#ebcb8b"
selection_opacity = 0.35

[terminal]
shell = "/bin/zsh"     # default: $SHELL
args = []
padding = 16
show_status = true

[[keybindings]]
command = "layout.splitVertical"
keys = "ctrl+alt+v"
```

User themes live next to the config: `~/.config/forge/themes/solar.toml` with
`base = "forge-light"` and any of `background`, `foreground`, `cursor`,
`selection`, `selection_opacity`, `accent`, `chrome`, `chrome_border`,
`chrome_active`, `chrome_active_border`, `muted`, `highlight`, `danger`,
`search_match`, `search_current`.

Commands (palette: `Ctrl+Shift+P`): `terminal.newTab` (`Ctrl+T`),
`terminal.newTabInDirectory`, `window.close` (`Ctrl+W`: closes the tab, or asks
before closing the last one), `window.toggleMaximize`, `layout.splitVertical`
(`Ctrl+\`), `layout.splitHorizontal` (`Ctrl+Shift+5`), `layout.focusNextPane`
(`Ctrl+Tab`), `theme.cycle` (`Ctrl+Shift+T`), `config.reload`,
`processExplorer.show`, `terminal.search` (`Ctrl+Shift+F`),
`terminal.searchNext` (`Ctrl+Shift+G`, towards older rows),
`terminal.searchPrevious` (`Ctrl+Shift+H`), `terminal.previousPrompt` /
`terminal.nextPrompt` (`Ctrl+Shift+Up`/`Down`, need shell integration),
`terminal.signal.interrupt` / `terminate` / `kill` (palette only),
`terminal.renameTab` (`Ctrl+Shift+R`), `terminal.moveTabLeft` / `Right`
(`Ctrl+Shift+PageUp`/`PageDown`), `view.zoomIn` / `zoomOut` / `zoomReset`
(`Ctrl+=`, `Ctrl+-`, `Ctrl+0`), `layout.focusPreviousPane` (`Ctrl+Shift+Tab`),
`layout.zoomPane` (`Ctrl+Shift+Enter`, shows only the active pane),
`layout.unsplit`, `terminal.newTabWithProfile`. The last window layout, size,
theme, tab names and zoom are restored from `session.json` next to the
config.

Profiles are named terminal setups for `terminal.newTabWithProfile`:

```toml
[[profiles]]
name = "python"
shell = "/usr/bin/python3"
args = ["-q"]
cwd = "~/proyectos"      # optional; defaults to the active tab's directory
env = { PYTHONSTARTUP = "/home/me/.pythonrc" }
```

Search: the bar searches the whole scrollback in `forge-termd` (literal and
case-insensitive by default; `Alt+R` regex, `Alt+C` case), highlights every
match, walks them with `Enter`/`Shift+Enter` and re-runs itself when the
terminal is resized or new output arrives.

Mouse: drag to select, double-click selects a word, triple-click a line; a
finished selection lands on the Linux primary buffer (middle click).
Keys: `Ctrl+Shift+C` copies, `Ctrl+Shift+V` pastes the clipboard,
`Shift+Insert` pastes the primary buffer. Typing clears the selection. A shell
that exits closes its tab; the last one closes the window.

Shell integration: bash, zsh and fish started by Forge (without custom
`terminal.args`) load `assets/shell-integration/` automatically
(`terminal.shell_integration = false` disables it; `$FORGE_SHELL_INTEGRATION`
points elsewhere). It emits OSC 133 prompt marks (a bar in the left margin of
every prompt row, prompt navigation) and OSC 7 (new tabs open in the current
directory). Other shells can `source "$FORGE_SHELL_INTEGRATION/<shell>/…"`.

Links: `Ctrl+hover` underlines OSC 8 hyperlinks, URLs and `path:line:col`
references; `Ctrl+click` opens them — URLs in the browser, files with
`terminal.open_file_command` (e.g. `["code", "-g", "{file}:{line}"]`) or the
desktop opener. Programs writing the clipboard (OSC 52) are confirmed once
per tab by default (`terminal.clipboard_write = "ask" | "allow" | "deny"`).
Pasting several lines into a program without bracketed paste, or text that
could escape a bracketed paste, asks first.

Diagnostics: `FORGE_LOG=debug` prints tracing/GPUI logs; `FORGE_TRACE_FILE=trace.json`
writes a Chrome trace of the startup spans that Perfetto can open.

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
- Phase 1 shell: tabs, splits, palette, keymap, themes, layered TOML config
  with schema, session restore, notifications, native dialogs, IME input,
  process explorer, tracing, and headless benchmarks in CI; the closure log
  with the pending manual tests is [`docs/PHASE_1_EXECUTION.md`](docs/PHASE_1_EXECUTION.md).
- Documentation: the Phase 0 close-out and the evidence expected from the
  Open VSX scan are recorded in [`docs/PHASE_0.md`](docs/PHASE_0.md).
- Next: Phase 2 terminal (scrollback, incremental redraw, ConPTY on Windows),
  ACP capability flow and benchmark history persistence. The Open VSX scanner
  is implemented; its first registry run remains an evidence-gathering task,
  not a claim of extension compatibility.
