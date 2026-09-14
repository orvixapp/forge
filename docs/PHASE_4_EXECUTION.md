# Ejecución de Fase 4 — Agentes (ACP) + MCP

Documento vivo. Alcance y presupuestos: [`ARCHITECTURE.md`](ARCHITECTURE.md)
§17 (agent host), §18 (MCP) y §29 (Fase 4). Punto de partida: el spike de
Fase 0 en `crates/proto-acp` (cliente ACP mínimo que habló con
`claude-code-acp`, Codex y Gemini CLI: `session/new` → `prompt` →
`fs/read_text_file` y `request_permission`).

## Estado heredado de las fases 1–3 (lo que ya existe y hay que reutilizar)

- Buffers del editor: `forge-buffer` (`Buffer`, `Transaction`, `replay`,
  journal). Las ediciones propuestas por un agente deben aplicarse como
  transacciones contra una versión (§17.5), nunca escribiendo a disco.
- Terminales: `forge-termd` por IPC (`proto-ipc` v7: `CreateSession` con
  `env`, `Attach`, `Key`, `Paste`, `Signal`…); `terminal/*` de ACP debe
  crear sesiones ahí y mostrarlas como pestañas.
- Pestañas: `Tab { content: Terminal | Editor }` en
  `crates/forge-gui/src/window.rs`; el panel del agente (`Ctrl+Shift+A`,
  §17.9) encaja como un tercer `TabContent`.
- Overlays y teclado: `overlay_key` en `window.rs`, comandos en
  `forge_gui::shell::ShellCommand` (id estable + título + keymap con
  contextos `Window`/`Terminal`/`Editor`).
- Config TOML con esquema (`docs/config.schema.json`, test que exige
  regenerarlo con `forge-gui --print-config-schema`).
- Convenciones: `cargo fmt`, `cargo clippy --workspace --all-targets --
  -D warnings`, tests; commits locales sin push; CI aparcada.

## 4.1 — Cliente ACP completo
- [x] `initialize`/`authenticate`/`session/new`/`session/load`/`prompt`/
  `cancel`/`set_mode`/`session/update` sobre stdio (JSON-RPC), con
  capacidades opcionales toleradas.
- [ ] Registro de agentes (`[[agents]]` en config + ACP registry) y
  arranque como proceso hijo supervisado (reinicio con backoff, §26).
  - [x] `[[agents]]`, registro validado, proceso hijo y reanudación con
    `session/load` tras reinicio con backoff.
  - [ ] Importar el registro oficial ACP y detección adicional en `PATH`.

## 4.2 — Panel de sesión
- [ ] `TabContent::Agent`: streaming virtualizado de mensajes, tool calls
  con estado, planes; entrada de prompt con contexto (@archivo, selección,
  salida de terminal).
  - [x] Tercer `TabContent`, streaming incremental y entrada de prompt.
  - [x] Tool calls con actualización de estado y planes.
  - [ ] Contexto visible (`@archivo`, selección y salida de terminal) y
    desplazamiento virtualizado navegable.
- [x] Línea de tiempo de herramientas (§17.9).

## 4.3 — fs/* y terminal/* sobre Forge
- [ ] `fs/read_text_file`/`fs/write_text_file` sobre buffers abiertos
  (texto vivo, no disco) con overlay de ediciones propuestas.
- [ ] `terminal/create|output|wait_for_exit|kill|release` sobre
  `forge-termd`, visibles como pestañas.

## 4.4 — Permisos y diff review
- [ ] `PermissionBroker`: política por agente/herramienta, tarjetas de
  permiso, decisiones recordadas por sesión.
- [ ] Overlay de ediciones propuestas + diff review por hunk + rebase
  sobre ediciones concurrentes (§17.5).

## 4.5 — Proveedores, router y acciones contextuales
- [ ] Perfiles de proveedor inyectados a Codex/OpenCode/Claude Code y
  router por clase de tarea con ruta visible y reenvío (§17.7).
- [ ] `agent.ask` (`Ctrl+K`) sobre la selección y `agent.investigate`
  desde un comando fallido en la terminal (§17.8).

## 4.6 — MCP y cierre
- [ ] Cliente MCP (config + passthrough) y `forge mcp-server` con las
  tools iniciales (§18); worktree por sesión.
- [ ] Benchmarks: overhead por `session/update` ≤ 0,2 ms; 10k tokens/min
  sin frames perdidos; 4 sesiones paralelas sin degradar el tecleo.
- [ ] Aceptación (§29): flujo completo «pedir cambio → ver diff → aceptar
  por hunk → tests en terminal creada por el agente» con los 4 agentes;
  Codex con login de ChatGPT sin configuración extra; tarea `trivial`
  atendida por un proveedor `free`.
