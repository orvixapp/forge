# Graph Report - .  (2026-09-13)

## Corpus Check
- Corpus is ~39,109 words - fits in a single context window. You may not need a graph.

## Summary
- 584 nodes · 1349 edges · 21 communities (20 shown, 1 thin omitted)
- Extraction: 99% EXTRACTED · 1% INFERRED · 0% AMBIGUOUS · INFERRED: 18 edges (avg confidence: 0.78)
- Token cost: 0 input · 0 output

## Community Hubs (Navigation)
- GUI Application Shell
- Grid Painting Pipeline
- Terminal Text Rendering
- Terminal Daemon Sessions
- Configuration Loading
- IPC Wire Protocol
- Benchmark Analysis
- ACP Workspace Bridge
- System Architecture
- Ghostty Terminal Wrapper
- Text Buffer Editing
- Ghostty C API
- Ghostty FFI Types
- Terminal CLI
- Daemon Integration Tests
- VS Code API Scanner
- FFI Resource Guards
- Render Snapshot Types
- Asset Loading Helpers
- Forge Visual Identity
- Ghostty Bootstrap Script

## God Nodes (most connected - your core abstractions)
1. `Api` - 28 edges
2. `ForgeWindow` - 26 edges
3. `Session` - 26 edges
4. `TerminalGrid` - 23 edges
5. `GhosttyTerminal` - 20 edges
6. `TerminalSurface` - 17 edges
7. `GlyphCache` - 17 edges
8. `Config` - 15 edges
9. `GhosttyError` - 15 edges
10. `spawn_io_threads()` - 15 edges

## Surprising Connections (you probably didn't know these)
- `Ghostty integration test` --conceptually_related_to--> `forge-termd persistent terminal daemon`  [INFERRED]
  .github/workflows/ci.yml → docs/ARCHITECTURE.md
- `Static vscode namespace member references` --rationale_for--> `VS Code API compatibility tiers`  [INFERRED]
  tools/vscode-api-scan/README.md → docs/ARCHITECTURE.md
- `Cross-platform Rust CI` --conceptually_related_to--> `Phase 0 prototype implementation`  [INFERRED]
  .github/workflows/ci.yml → README.md
- `Versioned benchmark budgets` --references--> `R1 R2 R3 reference machine profiles`  [INFERRED]
  README.md → bench/MACHINES.md
- `main()` --calls--> `resolve_font_family()`  [INFERRED]
  crates/forge-gui/src/main.rs → crates/forge-gui/src/config.rs

## Import Cycles
- None detected.

## Hyperedges (group relationships)
- **Forge agent protocol integration** — docs_architecture_forge, docs_architecture_acp, docs_architecture_mcp, docs_architecture_human_agent_environment [EXTRACTED 1.00]
- **VS Code extension compatibility strategy** — docs_architecture_open_vsx, docs_architecture_node_extension_host, docs_architecture_vscode_api_compatibility_tiers, docs_architecture_vscode_api_scanner [EXTRACTED 1.00]
- **Phase 0 architecture validation** — docs_architecture_phase_0_prototype, docs_architecture_gpui, docs_architecture_forge_termd, docs_architecture_vscode_api_scanner [EXTRACTED 1.00]
- **Forge Logo Visual Identity** — crates_forge_gui_assets_forge_logo_forge_logo, crates_forge_gui_assets_forge_logo_stylized_f_mark, crates_forge_gui_assets_forge_logo_lightning_bolt [EXTRACTED 1.00]

## Communities (21 total, 1 thin omitted)

### Community 0 - "GUI Application Shell"
Cohesion: 0.06
Nodes (63): AssetSource, ClipboardItem, Command, Context, Cow, Assets, cell_metrics_for(), chrome_height() (+55 more)

### Community 1 - "Grid Painting Pipeline"
Cohesion: 0.07
Nodes (55): Bounds, cell_index(), cell_metrics_size_the_element_from_the_grid_dimensions(), CellGlyph, CellMetrics, collect_glyph_cells(), ColorCache, GlyphCache (+47 more)

### Community 2 - "Terminal Text Rendering"
Cohesion: 0.07
Nodes (44): applies_full_frame_and_preserves_cell_metadata(), background_runs(), background_runs_merge_neighbours_and_skip_transparent_cells(), BackgroundRun, blank_cell(), cell(), CellPos, cursor_shape() (+36 more)

### Community 3 - "Terminal Daemon Sessions"
Cohesion: 0.10
Nodes (39): AtomicU16, append_bounded(), attach_session(), backlog_is_bounded_and_keeps_newest_bytes(), Daemon, dispatch(), handle_connection(), main() (+31 more)

### Community 4 - "Configuration Loading"
Cohesion: 0.12
Nodes (22): ColorConfig, Config, ConfigError, empty_document_yields_defaults_and_unknown_keys_are_rejected(), explicit_path_wins_over_environment(), FontConfig, HexColor, parses_partial_overrides_and_clamps_extremes() (+14 more)

### Community 5 - "IPC Wire Protocol"
Cohesion: 0.13
Nodes (29): buffered_reader_rejects_oversized_frames_before_buffering_them(), buffered_reader_survives_cancellation_mid_frame(), ClientMessage, CursorStyle, Frame, FrameKind, FrameReader, FrameReader<R> (+21 more)

### Community 6 - "Benchmark Analysis"
Cohesion: 0.12
Nodes (29): BenchmarkResult, percentile(), HashMap, Into, Option, String, Vec, ScenarioThreshold (+21 more)

### Community 7 - "ACP Workspace Bridge"
Cohesion: 0.17
Nodes (21): ClientSurface, jsonl_round_trip(), JsonRpcMessage, PermissionChoice, read_jsonl(), reads_an_unsaved_workspace_buffer_by_lines(), rejects_a_file_outside_the_workspace(), Error (+13 more)

### Community 8 - "System Architecture"
Cohesion: 0.08
Nodes (25): R1 R2 R3 reference machine profiles, Direct-paint GPUI grid element, Fully dirty 200 by 60 grid benchmark, Agent Client Protocol, Forge, forge-termd persistent terminal daemon, forge-text grid renderer, GPUI renderer (+17 more)

### Community 9 - "Ghostty Terminal Wrapper"
Cohesion: 0.24
Nodes (10): check(), GhosttyError, GhosttyTerminal, Result, Send, String, T, RawRenderState (+2 more)

### Community 10 - "Text Buffer Editing"
Cohesion: 0.23
Nodes (11): AppliedEdit, Buffer, BufferError, Edit, edit_undo_redo_round_trip_with_unicode(), invalid_edit_does_not_change_buffer(), Result, Self (+3 more)

### Community 11 - "Ghostty C API"
Cohesion: 0.10
Nodes (21): Api, FormatterFormatBuf, FormatterFree, FormatterNew, RenderStateClean, RenderStateFree, RenderStateGet, RenderStateNew (+13 more)

### Community 12 - "Ghostty FFI Types"
Cohesion: 0.17
Nodes (14): c_void, CursorStyle, DirtyState, formatter_options(), FormatterScreenExtra, FormatterTerminalExtra, FormatterTerminalOptions, GhosttyBuffer (+6 more)

### Community 13 - "Terminal CLI"
Cohesion: 0.27
Nodes (12): default_shell(), expect_initialized(), main(), Options, parse_args(), Option, PathBuf, R (+4 more)

### Community 14 - "Daemon Integration Tests"
Cohesion: 0.24
Nodes (12): collect_output(), command_output_crosses_the_daemon_boundary(), connect_when_ready(), ghostty_library(), Option, PathBuf, Result, String (+4 more)

### Community 15 - "VS Code API Scanner"
Cohesion: 0.32
Nodes (11): contributedKeys(), csv(), fetchTop(), findVsix(), main(), parseArgs(), registerUsage(), scanVsix() (+3 more)

### Community 16 - "FFI Resource Guards"
Cohesion: 0.22
Nodes (8): FormatterGuard, GhosttyLibrary, RowCellsGuard, RowIteratorGuard, Arc, Drop, RawFormatter, RawRowIterator

### Community 17 - "Render Snapshot Types"
Cohesion: 0.32
Nodes (8): RenderCell, RenderCursor, RenderRow, RenderSnapshot, CursorStyle, Option, Rgb, Vec

### Community 18 - "Asset Loading Helpers"
Cohesion: 0.50
Nodes (3): AsRef, Path, Self

### Community 19 - "Forge Visual Identity"
Cohesion: 0.50
Nodes (4): Forge Gradient, Forge Logo, Lightning Bolt, Stylized F Mark

## Knowledge Gaps
- **16 isolated node(s):** `GhosttyBuffer`, `GhosttyColorRgb`, `GhosttyRenderCursor`, `Rgb`, `Rgb` (+11 more)
  These have ≤1 connection - possible missing edges or undocumented components.
- **1 thin communities (<3 nodes) omitted from report** — run `graphify query` to explore isolated nodes.

## Suggested Questions
_Questions this graph is uniquely positioned to answer:_

- **Why does `write_message()` connect `IPC Wire Protocol` to `GUI Application Shell`, `Terminal Daemon Sessions`, `Benchmark Analysis`, `Terminal CLI`, `Daemon Integration Tests`?**
  _High betweenness centrality (0.197) - this node is a cross-community bridge._
- **Why does `Session` connect `Terminal Daemon Sessions` to `Ghostty Terminal Wrapper`?**
  _High betweenness centrality (0.154) - this node is a cross-community bridge._
- **Why does `GhosttyTerminal` connect `Ghostty Terminal Wrapper` to `FFI Resource Guards`, `Terminal Daemon Sessions`, `Ghostty C API`, `Ghostty FFI Types`?**
  _High betweenness centrality (0.129) - this node is a cross-community bridge._
- **What connects `GhosttyBuffer`, `GhosttyColorRgb`, `GhosttyRenderCursor` to the rest of the system?**
  _16 weakly-connected nodes found - possible documentation gaps or missing edges._
- **Should `GUI Application Shell` be split into smaller, more focused modules?**
  _Cohesion score 0.0620253164556962 - nodes in this community are weakly interconnected._
- **Should `Grid Painting Pipeline` be split into smaller, more focused modules?**
  _Cohesion score 0.07226107226107226 - nodes in this community are weakly interconnected._
- **Should `Terminal Text Rendering` be split into smaller, more focused modules?**
  _Cohesion score 0.06845238095238096 - nodes in this community are weakly interconnected._