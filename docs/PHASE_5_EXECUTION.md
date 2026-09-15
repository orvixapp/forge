# Ejecución de Fase 5 — LSP (Language Server Protocol)

Documento vivo. Alcance y presupuestos: [`ARCHITECTURE.md`](ARCHITECTURE.md)
§15 (LSP), §16 (Git), §17 (Agent Host), §18 (MCP), §26 (Crash recovery) y §29 (Fase 5).

## Corte de auditoría e integración vertical

El spike de Gemini ya no está aislado: `forge-gui/src/lsp.rs` mantiene un runtime
Tokio en un hilo dedicado y conecta snapshots Rope/versiones reales con
`forge-lsp`. GPUI intercambia resultados acotados (128 mensajes; 32 por poll),
diagnósticos visibles (200) y contadores precalculados. No espera procesos ni RPC.

- [x] Base 5.1: framing acotado, correlación, timeout incluyendo backpressure,
  cancelación al abandonar una petición, initialize/initialized/shutdown/exit.
- [x] Registro por lenguaje y configuración `[[languages]]` exclusivamente de usuario.
- [x] Base 5.2: snapshots vivos, didOpen versionado, cambios incrementales agrupados,
  didSave negociado, didClose y mapeo UTF-16 (emoji, CRLF y columnas fuera de línea).
- [x] Reutilización del watcher de archivos abiertos: eventos deduplicados a
  workspace/didChangeWatchedFiles por raíz, también cuando el buffer está sucio.
- [ ] Registro dinámico de globs y vigilancia recursiva de archivos no abiertos.
- [x] Flush antes de completion/hover/definition/signature help y descarte de resultados
  de otra revisión, posición o ruta; cancelación al mover cursor/cerrar documento.
- [x] Primera UI: completion con textEdit/additionalTextEdits transaccionales,
  fallback local, hover Markdown, F12 definición, signature help, lista navegable
  de diagnósticos, subrayado/gutter por severidad y contadores en estado.
- [x] Aceptación automatizada de rust-analyzer real por la misma ruta que usa la
  GUI (`LspManager` y el worker `LspService`): escribir, diagnósticos sin guardar
  por pull, hover, completion, F12, referencias, undo, rename, code actions,
  formateo y símbolos, con el disco intacto. Ver "Segunda vertical".
- [ ] Aceptación interactiva de rust-analyzer sobre el repositorio Forge y medición
  de fluidez (FPS) con la aplicación abierta.

## Segunda vertical: base cómoda y fiable

Hallazgos de la validación con rust-analyzer real y lo que se corrigió:

- rust-analyzer **no filtra** las sugerencias por prefijo: el worker filtra y
  ordena en cliente (prefijo exacto > prefijo sin mayúsculas > subsecuencia desde
  límite de palabra; desempate por `sortText`), acotado a 200 elementos.
- Los trigger characters (`.`, `:`, `(`…) se negocian con `completionProvider`
  del servidor: la GUI pide completion tras cualquier signo y el worker descarta
  la petición si el servidor no lo declara como trigger.
- Si el cliente anuncia diagnósticos pull, **rust-analyzer deja de hacer push**
  de los nativos (solo cargo check llega por push). El almacén indexa por
  fuente (`servidor/push`, `servidor/pull`) y fusiona; el worker hace pull en
  cada revisión sincronizada y reintenta con retroceso (0,5 s … 16 s) mientras
  un servidor recién arrancado responde vacío.
- El editor ya no borra los diagnósticos en cada tecla: los desplaza por la
  transacción (`Buffer::last_change`) hasta que llega la respuesta de la nueva
  revisión; un salto de versión que no puede seguir los descarta.
- Snippets LSP (`${1:x}`, `$0`, `${1|a,b|}`, variables, escapes): se expanden
  con sus valores por defecto, el primer tabstop queda seleccionado (multi-cursor
  si se repite) y Tab/Shift+Tab recorren el resto; Escape o un salto de
  versión terminan la sesión.
- `completionItem/resolve` en segundo plano al aceptar: la inserción es
  inmediata y los `additionalTextEdits` (auto-import) se aplican mapeados a
  través de lo escrito desde entonces; si solapan, se descartan.
- Referencias (Shift+F12), implementación (Ctrl+F12) y definiciones múltiples
  se listan con vista previa de línea; una sola ubicación salta directamente.
- Rename con preview (F2): `prepareRename` rellena el nombre, el `WorkspaceEdit`
  se resume por archivo y Enter lo aplica como transacciones (archivos no
  abiertos se abren; nada se escribe a disco). Operaciones de recursos
  (crear/renombrar/borrar archivos) se rechazan.
- Code actions (Ctrl+.): listado perezoso y `codeAction/resolve` al elegir;
  las acciones que son comandos del servidor se anuncian como no soportadas.
- Formateo (Shift+Alt+F) y `[lsp] format_on_save` (también en el menú de
  ajustes): un fallo del formateador nunca pierde el guardado.
- Agentes: herramientas MCP `forge_diagnostics` (opcionalmente por archivo) y
  `forge_workspace_symbols`; `agent.investigate` (Ctrl+Shift+I) desde el editor
  adjunta el fragmento, los diagnósticos de la línea, el hover y la definición
  del símbolo y `git diff` del archivo.

Pruebas: `forge-lsp/tests/rust_analyzer_manager_test.rs` (toda la vertical por
`LspManager`), `forge-gui/src/lsp_tests.rs::worker_drives_rust_analyzer_like_the_editor`
(el worker de la ventana con `LspService` real), `forge-gui/src/snippet.rs`,
pruebas del editor para desplazamiento de diagnósticos, sesiones de snippet y
mapeo de ediciones resueltas, y `forge-lsp/src/workspace_edit.rs`.

Correcciones de auditoría: el cliente ya no anuncia UTF-8 ni snippets que no sabe
aplicar; los servidores sólo reciben solicitudes de capacidades negociadas;
se liberan tareas/peticiones al cancelar; se detectan caídas también de procesos
en estado Running; los rangos de diagnóstico enormes no expanden miles de millones
de líneas; se ignoran diagnósticos versionados antiguos. El mock ahora aplica
cambios incrementales y no cancela lecturas de frames parciales al publicar eventos.

Pruebas: `forge-lsp/tests/regressions.rs`, suite mock y
`forge-lsp/tests/rust_analyzer_test.rs` (se omite sólo si no está en PATH).
La prueba real exige hover del símbolo añadido sin guardar y comprueba que el
archivo en disco sigue intacto, también después de parar/reiniciar el servidor.
Los tests de UI verifican el delta agrupado,
las ediciones de completion y la invalidación por Save As/cursor.

### Cómo probar esta primera vertical

Instala manualmente el servidor correspondiente y abre un archivo normal (no
modo archivo grande). Rust usa `rust-analyzer` automáticamente; TypeScript usa
`typescript-language-server --stdio`, Python `basedpyright-langserver --stdio`,
Go `gopls` y C/C++ `clangd`. No hay instalación automática.

- Escribir, un trigger (`.`, `::`) o Ctrl+Space: completion; Enter/Tab acepta
  mediante el buffer; en un snippet, Tab/Shift+Tab recorren los tabstops.
- Ctrl+Alt+H: hover; F12: definición; Shift+F12: referencias; Ctrl+F12:
  implementación; Ctrl+Shift+Space: signature help.
- Ctrl+Alt+M: lista de diagnósticos; ↑/↓ y Enter navegan, Esc cierra.
- F2: renombrar (con vista previa); Ctrl+.: acciones de código; Shift+Alt+F:
  formatear; Ctrl+Shift+I sobre un diagnóstico: `agent.investigate`.
- Todo lo anterior está también en el menú contextual del editor.

Ejemplo en el archivo **de usuario**, no `.forge/config.toml` del repositorio:

```toml
[lsp]
enabled = true
debounce_ms = 75
startup_ms = 500
idle_shutdown_secs = 1800
format_on_save = false

[[languages]]
id = "rust"
name = "Rust"
extensions = ["rs"]
[[languages.servers]]
name = "rust-analyzer"
command = "rust-analyzer"
args = []
root_markers = ["Cargo.toml", "rust-project.json", ".git"]
```

Desactivar `[lsp].enabled` mantiene el completado local. Los servidores ausentes
generan un aviso y se reintentan en segundo plano, sin impedir editar.
No se da por terminada toda la Fase 5: siguen abiertas la aceptación interactiva
sobre Forge y los presupuestos de FPS (no se infieren de tests), symbols de
documento en UI, `executeCommand`, registro dinámico de watchers, multi-servidor
real, tokens semánticos, inlay hints, folding, call hierarchy, process explorer
y la aceptación en TypeScript/Python/Go/C++.

## Estado heredado de las fases 1–4 (lo que ya existe y hay que reutilizar)

- Buffers del editor: `forge-buffer` (`Buffer`, `Transaction`, `Edit`, `replay`, `journal`).
  La sincronización `didChange` incremental se deriva de las transacciones del buffer (§15, §13.2).
- Editor y UI: `crates/forge-gui/src/editor.rs` y `crates/forge-gui/src/assist.rs`.
  Ya cuentan con popup de completion (`Completion`, `CompletionItem`), subrayado y conteo de
  diagnósticos (`Diagnostic`), y estado de sintaxis tree-sitter. LSP enriquece estos componentes
  y mantiene tree-sitter/palabras/esquema como fallback transparente.
- Configuración: `Config` en `forge-gui` con soporte de serialización, capas y validación
  con `docs/config.schema.json`.
- Integración con Agentes y MCP: `crates/forge-mcp` expone herramientas del editor.
  Fase 5 añade `forge_diagnostics` y `forge_workspace_symbols` (§18) y la acción contextual
  `agent.investigate` (§17.8) desde diagnósticos LSP.
- Pruebas y benchmarks: `crates/proto-bench` y suite de pruebas de workspace. Presupuesto:
  overhead de petición LSP ≤ 1 ms, 50k diagnósticos sin degradar tasa de cuadros.

---

## 5.1 — Cliente LSP base y transporte (`crates/forge-lsp`)
- [x] Nuevo crate `crates/forge-lsp` agregado al workspace de Cargo.
- [x] Transporte `Content-Length` asíncrono sobre stdio/AsyncRead/AsyncWrite;
  cabeceras ≤ 8 KiB y payload ≤ 16 MiB; no se crea un endpoint socket dedicado.
- [x] Cliente JSON-RPC con correlación de peticiones/respuestas por ID, notificaciones, errores tipados,
  timeouts y cancelación (`$/cancelRequest`).
- [x] Capacidades honestas para la vertical implementada; comprobación del
  proveedor antes de cada solicitud y selección explícita de UTF-16.
- [x] Proceso hijo supervisado: spawn con configuración de lenguaje, ciclo de vida (`initialize`,
  `initialized`, `shutdown`, `exit`), detección de raíz por `rootMarkers`.
- [x] Suite de pruebas con servidor mock (handshake, eco, notificaciones,
  cancelación y errores).

## 5.2 — Sincronización de documentos y mapeo posicional
- [x] Mapeo de coordenadas entre offset/char de `ropey::Rope` (UTF-8/char) y `lsp_types::Position` (UTF-16 line/col).
- [x] Notificaciones de ciclo de vida del documento: `textDocument/didOpen`, `textDocument/didSave`,
  `textDocument/didClose`.
- [x] Sincronización incremental `textDocument/didChange`:
  - Traducción de `forge_buffer::Edit` / transacciones a `TextDocumentContentChangeEvent`.
  - Control de versión monotónica (`VersionedTextDocumentIdentifier`).
  - Debounce de cambios (50–100 ms) con flush inmediato antes de peticiones que dependen del texto
    (completion, hover, definition).
- [x] Pruebas de integración de sincronización con servidor mock y validación contra `rust-analyzer`.

## 5.3 — Diagnósticos y navegación en la interfaz
- [x] Diagnósticos (`textDocument/publishDiagnostics` y soporte pull):
  - Almacén indexado por archivo, fuente y línea para consulta O(1); fusión de
    push y pull de varios servidores.
  - 50.000 diagnósticos indexados sin bloquear la UI (prueba de escala; la
    medición de FPS con la aplicación abierta sigue pendiente).
  - Renderizado en el editor: gutters, subrayado por severidad (Error, Warning, Info, Hint) y
    muestra en la barra de estado; los diagnósticos siguen a las ediciones.
- [x] Autocompletado LSP:
  - Petición `textDocument/completion` con trigger characters negociados.
  - Resolución perezosa (`completionItem/resolve`) tras aceptar.
  - Popup de `forge-gui` con snippets/insertText y ranking en cliente.
  - Fallback automático a `assist.rs` (palabras + tree-sitter + schema) si no hay servidor activo.
- [x] Hover (`textDocument/hover`): contenido Markdown en el overlay `LspInfo` (Ctrl+Alt+H).
  Pendiente: mostrarlo sobre el cursor y al pasar el ratón.
- [x] Navegación: `textDocument/definition`, `textDocument/references` e
  `implementation` abriendo el archivo/posición en el editor, con lista cuando hay varias.

## 5.4 — Formateo, acciones de código y símbolos
- [x] Formateo: `textDocument/formatting` y opción `format_on_save` (con la
  indentación por defecto del servidor). Pendiente: `rangeFormatting` en UI y
  tomar `tab_size`/`insert_spaces` del editor.
- [x] Acciones de código (`textDocument/codeAction`): quick fixes y refactors
  aplicados mediante transacciones (`WorkspaceEdit`), con `codeAction/resolve`.
  Pendiente: `workspace/executeCommand` para acciones que son comandos.
- [ ] Símbolos: `workspace/symbol` disponible para agentes (`forge_workspace_symbols`);
  pendiente `textDocument/documentSymbol` y un picker de símbolos en la UI.
- [ ] Prioridades y cancelación: cola priorizada por servidor (completion > hover/def > symbols).
  La cancelación al mover el cursor o cambiar de buffer ya existe.

## 5.5 — Multi-servidor, resiliencia y conexión MCP/Agente
- [x] Registro de lenguajes (`[[languages]]` en configuración de Forge):
  comandos, argumentos, variables de entorno, initializationOptions y rootMarkers.
- [ ] Multi-servidor: varios servidores por lenguaje (ej. compilador + linter) con fusión de
  capacidades (completions deduplicadas, hovers apilados, diagnósticos unificados).
- [x] Resiliencia y crash recovery (§26):
  - Apagado por inactividad tras tiempo configurable (`lsp.idle_shutdown_secs = 1800`).
  - Reinicio automático con retroceso exponencial ante caídas del servidor.
  - Reenvío automático de `didOpen` para todos los buffers abiertos tras el reinicio.
- [x] Integración MCP y Agente (§17.8, §18):
  - Herramienta MCP `forge_diagnostics` (filtrable por archivo o global en el workspace).
  - Herramienta MCP `forge_workspace_symbols`.
  - Acción contextual `agent.investigate` (`Ctrl+Shift+I`) desde un diagnóstico LSP en el editor,
    inyectando archivo, línea, diagnóstico, hover y definición del símbolo y git diff.
    Pendiente: lanzarla desde el gutter con el ratón.
- [ ] Benchmarks y validación final:
  - Benchmark `lsp_completion_overhead` ≤ 1 ms.
  - Verificación de 50k diagnósticos sin caída de FPS.
  - Verificación manual e interactiva con `rust-analyzer` sobre el propio repositorio Forge.

## Extensión posterior de Fase 5 (sin empezar Fase 6)

- [x] References/implementation en UI y selección entre múltiples definiciones.
- [x] Rename con preview y aplicación de WorkspaceEdit mediante transacciones
  (la comprobación de versión por documento de `documentChanges` no se exige aún).
- [ ] Folding LSP, semantic tokens, inlay hints y call hierarchy.
- [ ] Process explorer de servidores LSP.
- [ ] Aceptación real: Rust/rust-analyzer, TypeScript/typescript-language-server,
  Python/basedpyright, Go/gopls y C/C++/clangd.
