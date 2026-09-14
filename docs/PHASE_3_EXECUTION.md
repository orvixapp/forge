# Ejecución de Fase 3 — Editor core

Documento vivo de implementación y validación. Alcance y presupuestos:
[`ARCHITECTURE.md`](ARCHITECTURE.md) §13 y §29 (Fase 3). Sin LSP, sin git,
sin snippets, sin emulación Vim completa.

## 3.1 — Buffer: rope, transacciones, undo y journal (`forge-buffer`)

- [x] Rope `ropey` con snapshots O(1) para hilos de fondo (`Buffer::rope`).
  Posiciones en chars; `byte_of`/`char_of` para tree-sitter y LSP;
  `Position { line, column }` con clamp.
- [x] `Transaction { edits, selections_before, selections_after, version_before }`
  validada (rangos, solapes), aplicada en orden inverso, con inversa
  calculada al aplicar y `map_position` para reubicar posiciones.
- [x] Undo/redo como pila de transacciones con inversas; el tecleo se
  agrupa por tiempo (300 ms) componiendo transacciones mediante un modelo
  de piezas (proptest: componer ≡ aplicar en secuencia). Undo tree
  **[ABIERTO]** sigue abierto: la pila permite convertirse en árbol después.
- [x] Selecciones modelo Helix (`Selections`: ordenadas, fusionadas,
  primaria estable); `insert`/`delete` operan sobre todas las selecciones.
- [x] Journal append-only por buffer (`Journal`): una línea JSON por
  transacción, fsync ≤ 1 s, truncado al guardar, lectura tolerante a la
  última línea cortada, `replay` con comprobación de versión.
- [x] Archivos: BOM/UTF-8/UTF-16/windows-1252 (`encoding_rs`),
  normalización de EOL con restauración al guardar, escritura atómica.
- [x] 17 pruebas unitarias + 2 propiedades (`proptest`): undo restaura todo
  estado previo; composición de ediciones.
- [ ] Bench `forge-bench buffer_typing` (tecleo sintético 1.000 chars en
  10k líneas) cuando exista la vista con highlighting (el presupuesto de
  3 ms p99 es de tecla a frame).

## 3.2 — Vista de editor en la GUI

- [x] `Tab { content: Terminal | Editor }` en el mismo árbol de paneles y
  la misma sesión (`files` en `session.json`); abrir con `Ctrl+O`, nuevo
  con `Ctrl+N`, guardar con `Ctrl+S` (guardar como para sin título), CLI
  `forge-gui ruta[:línea[:col]]`, `Ctrl+clic` en `ruta:línea` desde la
  terminal abre el editor; cerrar una pestaña sucia pregunta
  (guardar/descartar/cancelar).
- [x] `EditorElement`: líneas visibles con `shape_line`, gutter con
  números, cursores y selecciones, barra de scroll, rueda y teclado.
- [x] Edición: tecleo e IME, Backspace/Delete (y `Ctrl+Backspace` por
  palabra), Enter con indentación heredada, Tab (espacios o `\t`),
  movimiento por chars/palabras/líneas/página/documento con selección por
  `Shift`, ratón (clic, arrastre, doble/triple clic, `Shift`/`Alt`+clic),
  copiar/cortar/pegar (línea completa con selección vacía), undo/redo,
  multi-cursor (`Ctrl+Alt+↑/↓`, `Ctrl+D`), `Esc` colapsa.
- [x] Journal por archivo bajo `<config>/journal/` mientras el buffer
  está sucio; al abrir un archivo con journal pendiente se reproduce y se
  avisa («recuperado del journal»).
- [ ] Wrap opcional, scroll horizontal y resaltado de la línea actual.
- [ ] «Recuperar N archivos» al arrancar para archivos que no se vuelven a
  abrir (hoy sólo al abrirlos).

## 3.3 — Sintaxis (`forge-syntax`)

- [ ] tree-sitter con gramáticas cargadas bajo demanda, parseo incremental
  en hilo de fondo sobre snapshot, `highlights.scm`/`injections.scm`,
  caché por línea invalidada por `changed_ranges`, mapeo captura → token
  del tema; folds e indents.

## 3.4 — Proyecto y búsqueda (`forge-project`, `forge-search`)

- [ ] Explorador de archivos con `ignore` y exclusiones globales; fuzzy
  finder (`nucleo` + `fst`); búsqueda/reemplazo en buffer (regex) y en
  proyecto (crates de ripgrep) en stream; watcher (`notify`) con fallback.

## 3.5 — Archivos grandes, autosave y cierre

- [ ] Modo grande (> 200 MB o línea > 1 MB): `mmap`, índice de líneas lazy,
  sin tree-sitter, sólo lectura hasta la primera edición.
- [ ] Autosave configurable; keymap modal Helix-like básico.
- [ ] Benchmarks: tecleo p99 ≤ 3 ms (10k líneas con highlighting), scroll
  60/120 fps, `open_1gb_log` ≤ 500 ms, `search_linux_literal` ≤ 1,2× rg,
  `fuzzy_1m_paths` ≤ 10 ms, RSS con `linux` ≤ 250 MB.
- [ ] Aceptación: editar Forge con Forge; `kill -9` recupera buffers;
  suite proptest verde.
