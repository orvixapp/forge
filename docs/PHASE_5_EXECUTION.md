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
- [ ] Aceptación interactiva de rust-analyzer sobre Forge y medición de fluidez.

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

- Escribir o Ctrl+Space: completion; Enter/Tab acepta mediante el buffer.
- Ctrl+Alt+H: hover; F12: definición; Ctrl+Shift+Space: signature help.
- Ctrl+Alt+M: lista de diagnósticos; ↑/↓ y Enter navegan, Esc cierra.

Ejemplo en el archivo **de usuario**, no `.forge/config.toml` del repositorio:

```toml
[lsp]
enabled = true
debounce_ms = 75
startup_ms = 500
idle_shutdown_secs = 1800

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
No se da por terminada toda la Fase 5: las casillas combinadas de abajo siguen
abiertas cuando falta alguna subfunción (pull, snippets, references visibles,
MCP, tokens semánticos, etc.). Los presupuestos de FPS no se infieren de tests.

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
- [x] Registro de lenguajes (`[[languages]]` en configuración de Forge):
  comandos, argumentos, variables de entorno, initializationOptions y rootMarkers.
- [ ] Multi-servidor: varios servidores por lenguaje (ej. compilador + linter) con fusión de
  capacidades (completions deduplicadas, hovers apilados, diagnósticos unificados).
- [x] Resiliencia y crash recovery (§26):
  - Apagado por inactividad tras tiempo configurable (`lsp.idle_shutdown_secs = 1800`).
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

## Extensión posterior de Fase 5 (sin empezar Fase 6)

- [ ] References/implementation en UI y selección entre múltiples definiciones.
- [ ] Rename con preview y aplicación versionada de WorkspaceEdit.
- [ ] Folding LSP, semantic tokens, inlay hints y call hierarchy.
- [ ] Process explorer de servidores LSP.
- [ ] Aceptación real: Rust/rust-analyzer, TypeScript/typescript-language-server,
  Python/basedpyright, Go/gopls y C/C++/clangd.
