# Prueba manual de Fase 4

La implementación 4.1–4.3 ya es utilizable. Hace falta un agente que hable ACP;
Token Harbor es opcional y solamente cambia el proveedor de modelos que usa ese
agente.

## Camino corto con OpenCode

En esta máquina `opencode` ya está instalado. Añadir al archivo de usuario
`~/.config/forge/config.toml` (no al workspace):

```toml
[[agents]]
name = "OpenCode"
command = "opencode"
args = ["acp"]
enabled = true
```

Después:

1. Ejecutar `cargo run -p forge-gui` desde el workspace que se quiere abrir.
2. Pulsar `Ctrl+Shift+A`, o ejecutar `agent.newSession` desde `Ctrl+Shift+P`.
3. Enviar un prompt que pida leer un archivo abierto y ejecutar un comando
   corto, por ejemplo: `Lee @README.md y ejecuta printf forge-acp`.
4. Verificar que la respuesta llega en streaming y que la herramienta aparece
   en la línea de tiempo.
5. Hacer un cambio sin guardar en `README.md` y pedir que lea ese archivo. El
   agente debe recibir el texto del buffer, no la versión del disco.
6. Pedir una edición (`fs/write_text_file`). Debe aparecer como propuesta en el panel
   del agente y abrir el visor de diff con los hunks calculados mediante Myers diff.
   Verificar que no modifica el archivo en disco ni el buffer directamente.
7. En la tarjeta de revisión de diff:
   - Revisar cada hunk individualmente con sus líneas afectadas (`+` y `-`).
   - Aceptar un hunk con "Aceptar": se aplica como `Transaction` en el buffer del editor
     (admitiendo deshacer con `Ctrl+Z`) y el hunk pasa a estado "Aceptado".
   - Rechazar un hunk con "Rechazar": el hunk pasa a estado "Rechazado" sin modificar el buffer.
   - Usar "Aceptar todo" (`agent.acceptAllHunks`) o "Rechazar todo" (`agent.rejectAllHunks`)
     desde los botones o desde la paleta de comandos (`Ctrl+Shift+P`).
8. Si se modifica el buffer concurrentemente mientras hay una propuesta pendiente:
   - Los hunks no afectados se rebasan limpiamente recalculando offsets.
   - Los hunks cuyas líneas coincidan con la edición concurrente se marcan con
     `Conflicto: El contenido del buffer no coincide con la versión base`, impidiendo
     su aplicación hasta resolver el conflicto.
9. Cuando el agente solicita ejecutar comandos o herramientas que requieren permiso
   (`session/request_permission`):
   - Aparece una tarjeta de permiso en el panel del agente con botones:
     "Permitir una vez", "Permitir en esta sesión", "Permitir siempre", "Rechazar".
   - La solicitud espera la decisión del usuario antes de responder al agente.
   - "Permitir en esta sesión" recuerda la decisión para la herramienta/comando en la
     sesión activa sin volver a preguntar.
   - "Permitir siempre" persiste la regla en
     `~/.config/forge/permissions/<workspace>-<hash>.json` (nunca dentro del
     repositorio: un clon no puede traer permisos). Un "Permitir siempre"
     sobre `execute`/terminal/red sin comando concreto se aplica sólo una
     vez.
   - "Rechazar" responde negativamente al agente con la opción de denegación.
10. Cuando el agente ejecute el comando autorizado, se abre la pestaña
    `Agente · <comando>`; `terminal/output` devuelve su salida y la pestaña permanece
    visible al terminar. `terminal/release` la cierra.

## Token Harbor como proveedor opcional

El dominio correcto es `tokenharbor.ai`. Token Harbor no sustituye ACP ni se
añade como `[[agents]]`: es un proveedor (`[[providers]]`) que Forge inyecta
como entorno al agente que ya lanza. Endpoints (docs oficiales):
`https://tokenharbor.ai/v1` compatible con OpenAI (Codex, OpenCode) y
`https://tokenharbor.ai` compatible con Anthropic (Claude Code); la clave
`thk_…` sale del dashboard y viaja como `Bearer`.

```toml
# ~/.config/forge/config.toml — export TOKENHARBOR_API_KEY=thk_... en la shell
[[providers]]
name = "tokenharbor"
kind = "openai-compatible"          # Codex y OpenCode
model = "th-orchestra"              # o un id explícito: tokenharbor/qwen3-max…
base_url = "https://tokenharbor.ai/v1"
api_key_env = "TOKENHARBOR_API_KEY"

[[providers]]
name = "tokenharbor-claude"
kind = "anthropic"                  # Claude Code (ANTHROPIC_BASE_URL + AUTH_TOKEN)
model = "th-orchestra"
base_url = "https://tokenharbor.ai"
api_key_env = "TOKENHARBOR_API_KEY"

[router]
trivial = "tokenharbor"
normal = "tokenharbor"
deep = "tokenharbor"
```

Lo que Forge no puede hacer desde fuera es elegir el proveedor dentro de la
configuración propia de cada agente: OpenCode necesita el bloque `provider`
de `~/.config/opencode/opencode.json` y Codex `model_provider` en
`~/.codex/config.toml` (`tokenharbor connect opencode|codex|claude` los
escribe). Con eso hecho, el entorno que inyecta Forge aporta la clave y la
URL y la ruta del panel muestra `tokenharbor`.

1. Instalar y ejecutar el conector siguiendo la documentación oficial de Token
   Harbor.
2. Elegir solamente `opencode`, `codex` o `claude`, según el agente configurado
   en Forge. La clave se introduce en el prompt del conector; nunca se guarda en
   `config.toml` ni se pasa como argumento de proceso.
3. Comprobar la conexión con `tokenharbor status` y, si hace falta,
   `tokenharbor doctor`.
4. Reiniciar Forge para que el agente lea su configuración actualizada y repetir
   la prueba anterior.
5. Para volver al proveedor nativo, ejecutar `tokenharbor disconnect opencode`
   (o el nombre del agente seleccionado).

El conector oficial añade un provider a Codex y OpenCode, y configura las
variables del gateway para Claude Code. Esto mantiene a Forge independiente del
servicio: si Token Harbor no está instalado o conectado, el agente usa su
proveedor nativo.

## Otros agentes ACP

El registro oficial incluye, entre otros, Codex, Claude Agent, Gemini CLI y
OpenCode. Forge detecta ejecutables ya instalados en `PATH`; no descarga agentes
automáticamente. También se puede declarar cualquier adaptador explícitamente
con `[[agents]]`, indicando su comando ACP y sus argumentos.

## Proveedores, router y reenvío (4.5)

1. Declara dos proveedores y un router en `~/.config/forge/config.toml`
   (ejemplo en el README: uno `free = true` para `trivial`, otro para
   `normal`/`deep`) y exporta la clave que nombra `api_key_env`.
2. `Ctrl+Shift+A`: el panel muestra «Ruta: normal → <proveedor>: <modelo>».
   En el adaptador (`opencode acp` con `FORGE_LOG=debug`) deben verse
   `OPENAI_BASE_URL`/`OPENAI_MODEL` (o `ANTHROPIC_*`/`GEMINI_*`) del
   proveedor; sin `[router]` ni `agents[].provider` no se inyecta nada y el
   agente usa su login (Codex con ChatGPT).
3. Escribe `/trivial resume este archivo` y Enter: se abre una sesión nueva
   con la ruta `trivial → <proveedor free>` y el prompt ya enviado; la
   sesión original registra «Reenviado a …». Con `/normal` en una sesión
   que ya usa ese proveedor el prompt se envía en la misma sesión sin el
   prefijo.
4. Paleta → «Forward the last prompt to another provider…» (`agent.forward`):
   elige un proveedor y comprueba la nueva sesión.
5. `agents[].worktree = true`: la ruta termina en «· worktree
   <config>/worktrees/<agente>-<ts>» y `git worktree list` en el repo lo
   muestra con la rama `forge/<agente>-<ts>`; los archivos que el agente
   propone se resuelven contra ese directorio. Sin git en el cwd aparece
   un aviso y la sesión usa el directorio actual.

## `agent.ask` y `agent.investigate`

1. En un editor selecciona unas líneas y pulsa `Ctrl+K`: aparece el
   cuadro «Pregunta al agente sobre <archivo>»; escribe la pregunta (admite
   prefijo `/deep`) y Enter. La sesión nueva lleva la selección como chip
   de contexto y el prompt ya enviado. Sin archivo abierto avisa.
2. En una terminal ejecuta un comando que falle (`cargo build` con un error,
   `ls /nope`) y pulsa `Ctrl+Shift+I` (o botón derecho → «Investigate the
   last failed command…»). El prompt contiene el directorio y el comando;
   el chip `terminal://<sesión>` lleva la salida entre la última marca de
   prompt y el cursor. Sin integración de shell avisa y envía la pantalla
   visible.

## MCP (4.6)

1. Con Forge abierto, en una terminal de Forge: `echo $FORGE_GUI_SOCKET`
   apunta a `$XDG_RUNTIME_DIR/forge-gui-<pid>.sock`.
2. Servidor a mano: `printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}' '{"jsonrpc":"2.0","id":2,"method":"tools/list"}' '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"forge_list_open_files"}}' | forge-gui mcp-server`
   responde `protocolVersion`, las cinco tools y los archivos abiertos
   (con `dirty` verdadero si tienen cambios sin guardar).
3. `forge_read_buffer` de un archivo abierto con cambios sin guardar
   devuelve el texto vivo, no el del disco.
4. `forge_run_in_terminal` con `{"command":"echo hola"}`: aparece una
   tarjeta de permiso en la sesión de agente (la primera si el servidor se
   lanzó a mano); «Permitir» devuelve `exit: 0` y la salida; «Rechazar»
   devuelve `isError`. Sin sesión de agente abierta responde error, no
   se ejecuta nada.
5. `forge_propose_edit` sobre un archivo abierto crea la propuesta en el
   panel del agente para revisar por hunk; el archivo no cambia hasta
   aceptar.
6. Con un agente ACP: en `session/new` (log del adaptador) `mcpServers`
   lleva `forge` primero y después los `[[mcp_servers]]` de la config; el
   agente lista las tools `forge_*` (`/mcp` en Claude Code, `mcp list` en
   OpenCode).

## Benchmarks (4.6)

```bash
cargo build --release -p forge-gui -p forge-bench
cargo run --release -p forge-bench -- agent_update_overhead --iterations 5000 --check
cargo run --release -p forge-bench -- agent_stream --iterations 300 --check
cargo run --release -p forge-bench -- agent_parallel_typing --iterations 300 --check
cargo run --release -p forge-bench -- editor_typing --iterations 300   # referencia
```

Resultados de referencia en `bench/results/2026-09-14-agent.md`.
