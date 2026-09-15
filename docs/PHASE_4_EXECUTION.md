# Ejecución de Fase 4 — Agentes (ACP) + MCP

Documento vivo. Alcance y presupuestos: [`ARCHITECTURE.md`](ARCHITECTURE.md)
§17 (agent host), §18 (MCP) y §29 (Fase 4). Punto de partida: el spike de
Fase 0 en `crates/proto-acp` (cliente ACP mínimo que habló con
`claude-code-acp`, Codex y Gemini CLI: `session/new` → `prompt` →
`fs/read_text_file` y `request_permission`).

Pruebas manuales de las entregas ya utilizables: [`PHASE_4_TESTING.md`](PHASE_4_TESTING.md).

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
- [x] Registro de agentes (`[[agents]]` en config + ACP registry) y
  arranque como proceso hijo supervisado (reinicio con backoff, §26).
  - [x] `[[agents]]`, registro validado, proceso hijo y reanudación con
    `session/load` tras reinicio con backoff.
  - [x] Importar el registro oficial ACP y detección adicional en `PATH`.

## 4.2 — Panel de sesión
- [x] `TabContent::Agent`: streaming virtualizado de mensajes, tool calls
  con estado, planes; entrada de prompt con contexto (@archivo, selección,
  salida de terminal).
  - [x] Tercer `TabContent`, streaming incremental y entrada de prompt.
  - [x] Tool calls con actualización de estado y planes.
  - [x] Contexto visible (`@archivo`, selección y salida de terminal) y
    desplazamiento virtualizado navegable.
- [x] Línea de tiempo de herramientas (§17.9).

## 4.3 — fs/* y terminal/* sobre Forge
- [x] `fs/read_text_file`/`fs/write_text_file` sobre buffers abiertos
  (texto vivo, no disco) con overlay de ediciones propuestas.
- [x] `terminal/create|output|wait_for_exit|kill|release` sobre
  `forge-termd`, visibles como pestañas.

## 4.4 — Permisos y diff review
- [x] `PermissionBroker`: política por agente/herramienta, tarjetas de
  permiso, decisiones recordadas por sesión y persistencia por workspace en
  el directorio de config del usuario (`permissions/<workspace>-<hash>.json`);
  la capability sale de `toolCall.kind`, los patrones de comando casan por
  tokens y los globs son reales; sin opción `reject_*` se responde
  `cancelled`, nunca un `allow_*`.
- [x] Overlay de ediciones propuestas + diff review por hunk + rebase
  sobre ediciones concurrentes (§17.5) aplicadas como `forge-buffer::Transaction`.

## 4.5 — Proveedores, router y acciones contextuales
- [x] Perfiles de proveedor inyectados a Codex/OpenCode/Claude Code y
  router por clase de tarea con ruta visible y reenvío (§17.7).
  - `[[providers]]` (`kind` openai/anthropic/google/openai-compatible,
    `model`, `base_url`, `api_key_env`, `free`) → variables de entorno del
    adaptador (`OPENAI_*`, `ANTHROPIC_*`, `GEMINI_*`, `FORGE_PROVIDER*`,
    `FORGE_TASK_CLASS`); la clave se lee del entorno, nunca se guarda.
  - `[router]` `trivial|normal|deep` → proveedor; `agents[].provider` como
    respaldo; sin proveedor el adaptador conserva su propio login.
  - Ruta visible en el panel («Ruta: normal → gpt: gpt-5 · worktree …»);
    prefijos `/trivial|/normal|/deep` en el prompt; si otro proveedor
    atiende la clase se abre una sesión nueva y ambas lo indican;
    `agent.forward` reenvía el último prompt al proveedor elegido.
- [x] `agent.ask` (`Ctrl+K`) sobre la selección y `agent.investigate`
  (`Ctrl+Shift+I` y menú contextual de la terminal) desde el último comando,
  delimitado por las marcas OSC 133 del grid visible, con salida y cwd.

## 4.6 — MCP y cierre
- [x] Cliente MCP (config + passthrough) y `forge mcp-server` con las
  tools iniciales (§18); worktree por sesión.
  - `[[mcp_servers]]` → `session/new.mcpServers` (`{name, command, args,
    env:[{name,value}]}`); el agente conecta directamente, Forge no proxya.
  - `crates/forge-mcp`: `forge-gui mcp-server` (stdio, JSON-RPC por
    líneas, `initialize`/`tools/list`/`tools/call`/`resources/*`) y puente
    por socket Unix (`FORGE_GUI_SOCKET`, `FORGE_AGENT_TAB`,
    `FORGE_WORKSPACE`) hacia la ventana, reutilizando los handlers ACP
    (`fs/*`, `session/request_permission`). Tools: `forge_list_open_files`,
    `forge_read_buffer`, `forge_git_status`, `forge_run_in_terminal` (con
    tarjeta de permiso), `forge_propose_edit` (overlay §17.5); recursos
    `forge://buffer/<path>`. Los nombres usan `_` porque MCP restringe los
    nombres a `[A-Za-z0-9_-]`. Forge se añade como primer servidor de cada
    sesión y exporta el socket a las terminales.
  - `agents[].worktree = true`: `git worktree add -b forge/<agente>-<ts>
    <config>/worktrees/<agente>-<ts> HEAD`; la sesión, sus `fs/*`, MCP y
    terminales usan ese directorio.
  - Pendiente: `forge/diagnostics` y `forge/workspace_symbols` (requieren
    LSP, fase 5); endpoint HTTP local con token; MCP Streamable HTTP como
    cliente.
- [x] Benchmarks (`bench/results/2026-09-14-agent.md`): `agent_update_overhead`
  p95 0,001 ms (≤ 0,2); `agent_stream` p95 2,4 ms por frame a 10k
  tokens/min (sin frames > 16,6 ms); `agent_parallel_typing` 3,5 ms
  mediana / 4,8 ms p95 frente a `editor_typing` 3,4 / 4,5 ms en la misma
  máquina y momento (dentro del ruido; ambos por encima del umbral de 3 ms
  con la máquina cargada, como ya se anotó en Fase 3).
- [ ] Aceptación (§29): flujo completo «pedir cambio → ver diff → aceptar
  por hunk → tests en terminal creada por el agente» con los 4 agentes;
  Codex con login de ChatGPT sin configuración extra; tarea `trivial`
  atendida por un proveedor `free`. Guion en `PHASE_4_TESTING.md`; requiere
  ejecutar los agentes reales (validación manual del usuario).

## Ampliación 4.6 — Configuración MCP común (2026-09-15)

- [x] Definiciones compartidas por todos los agentes ACP, con `enabled` y
  lista `agents` por servidor; vacía significa todos. Ajustes (`Ctrl+,`):
  importar, gestionar activación/acceso y añadir MCP HTTP.
- [x] Importación explícita desde Claude Code (`.mcp.json` o
  `~/.claude.json`, incluido el scope local del workspace), Codex
  (`$CODEX_HOME/config.toml`) y OpenCode (`opencode.json`/`opencode.jsonc`).
  Solo lectura del origen; imports desactivados, deduplicación por nombre
  y conexión; comentarios del destino preservados. No se sobrescriben
  ajustes abiertos con cambios sin guardar.
- [x] Transporte HTTP en passthrough: comprobación de
  `agentCapabilities.mcpCapabilities.http` después de `initialize`, HTTPS
  o HTTP loopback únicamente, sin credenciales en URL. Headers mediante
  referencias a variables de entorno; aviso en timeline si falta una
  variable o el agente no anuncia HTTP. Sin proxy ni instalación automática.
- [x] No importar sesiones OAuth ni almacenes de credenciales. Valores
  literales en `env`/headers, argumentos con flags de credenciales, SSE y
  restricciones sin equivalente requieren migración manual y se omiten
  con aviso seguro. Referencias `${VARIABLE}` y `{env:VARIABLE}` se
  normalizan; las claves se resuelven únicamente al crear la sesión.
- [ ] Validación manual con servidores/agentes reales: importación,
  autorización independiente y herramientas disponibles según allowlist.
- [ ] Cliente HTTP/OAuth propio de Forge (listar tools/resources/prompts,
  conectar/desconectar, renovación y almacenamiento seguro). Esta entrega
  configura conexiones directas del agente; **no implementa** ese cliente
  ni copia el login de otro CLI. OAuth lo autoriza cada cliente por separado.
