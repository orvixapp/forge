# Ejecución de Fase 5 — LSP (Language Server Protocol)

Documento vivo. Alcance y presupuestos: [`ARCHITECTURE.md`](ARCHITECTURE.md)
§15 (LSP), §16 (Git), §17 (Agent Host), §18 (MCP), §26 (Crash recovery) y §29 (Fase 5).

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
- [ ] Nuevo crate `crates/forge-lsp` agregado al workspace de Cargo.
- [ ] Transporte `Content-Length` asíncrono sobre stdio (y sockets) con framing estricto JSON-RPC 2.0.
- [ ] Cliente JSON-RPC con correlación de peticiones/respuestas por ID, notificaciones, errores tipados,
  timeouts y cancelación (`$/cancelRequest`).
- [ ] Negociación de `ClientCapabilities` completa (sync incremental, diagnostics, completion,
  hover, definition, references, formatting, codeAction, workspace symbols).
- [ ] Proceso hijo supervisado: spawn con configuración de lenguaje, ciclo de vida (`initialize`,
  `initialized`, `shutdown`, `exit`), detección de raíz por `rootMarkers`.
- [ ] Suite de pruebas unitarias exhaustiva con servidor mock (handshake, eco, notificaciones,
  cancelación y errores).

## 5.2 — Sincronización de documentos y mapeo posicional
- [ ] Mapeo de coordenadas entre offset/char de `ropey::Rope` (UTF-8/char) y `lsp_types::Position` (UTF-16 line/col).
- [ ] Notificaciones de ciclo de vida del documento: `textDocument/didOpen`, `textDocument/didSave`,
  `textDocument/didClose`.
- [ ] Sincronización incremental `textDocument/didChange`:
  - Traducción de `forge_buffer::Edit` / transacciones a `TextDocumentContentChangeEvent`.
  - Control de versión monotónica (`VersionedTextDocumentIdentifier`).
  - Debounce de cambios (50–100 ms) con flush inmediato antes de peticiones que dependen del texto
    (completion, hover, definition).
- [ ] Pruebas de integración de sincronización con servidor mock y validación contra `rust-analyzer`.

## 5.3 — Diagnósticos y navegación en la interfaz
- [ ] Diagnósticos (`textDocument/publishDiagnostics` y soporte pull):
  - Almacén de diagnósticos indexado por archivo y línea para consulta O(1).
  - Capacidad para procesar hasta 50.000 diagnósticos sin bloquear el hilo de UI ni perder frames.
  - Renderizado en el editor: gutters, subrayado por severidad (Error, Warning, Info, Hint) y
    muestra en la barra de estado.
- [ ] Autocompletado LSP:
  - Petición `textDocument/completion` con soporte para trigger characters.
  - Resolución perezosa (`completionItem/resolve`).
  - Integración en el popup de `forge-gui` con snippets/insertText y ranking.
  - Fallback automático a `assist.rs` (palabras + tree-sitter + schema) si no hay servidor activo.
- [ ] Hover (`textDocument/hover`): contenido Markdown formateado en popup contextual sobre el cursor.
- [ ] Navegación: `textDocument/definition` y `textDocument/references` abriendo el archivo/posición
  correspondiente en el editor.

## 5.4 — Formateo, acciones de código y símbolos
- [ ] Formateo: `textDocument/formatting`, `textDocument/rangeFormatting` y opción `format_on_save`.
- [ ] Acciones de código (`textDocument/codeAction`): quick fixes y refactors aplicados mediante
  transacciones (`WorkspaceEdit`).
- [ ] Símbolos: `textDocument/documentSymbol` para navegación rápida en archivo y `workspace/symbol`
  para búsqueda difusa de símbolos en todo el proyecto.
- [ ] Prioridades y cancelación: cola priorizada por servidor (completion > hover/def > symbols) y
  cancelación automática al mover el cursor o cambiar de buffer.

## 5.5 — Multi-servidor, resiliencia y conexión MCP/Agente
- [ ] Registro de lenguajes (`languages.toml` / `[[languages]]` en configuración de Forge):
  comandos, argumentos, variables de entorno, initializationOptions y rootMarkers.
- [ ] Multi-servidor: varios servidores por lenguaje (ej. compilador + linter) con fusión de
  capacidades (completions deduplicadas, hovers apilados, diagnósticos unificados).
- [ ] Resiliencia y crash recovery (§26):
  - Apagado por inactividad tras tiempo configurable (`lsp.idle_shutdown = 30m`).
  - Reinicio automático con retroceso exponencial ante caídas del servidor.
  - Reenvío automático de `didOpen` para todos los buffers abiertos tras el reinicio.
- [ ] Integración MCP y Agente (§17.8, §18):
  - Herramienta MCP `forge_diagnostics` (filtrable por archivo o global en el workspace).
  - Herramienta MCP `forge_workspace_symbols`.
  - Acción contextual `agent.investigate` (`Ctrl+Shift+I`) desde un diagnóstico LSP en el editor
    o gutter, inyectando archivo, línea, diagnóstico, definición del símbolo y git diff.
- [ ] Benchmarks y validación final:
  - Benchmark `lsp_completion_overhead` ≤ 1 ms.
  - Verificación de 50k diagnósticos sin caída de FPS.
  - Verificación manual e interactiva con `rust-analyzer` sobre el propio repositorio Forge.
