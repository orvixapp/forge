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
- [x] Bench `forge-bench editor_typing` (tecleo sintético en 10k líneas
  con resaltado, tecla → present): ver 3.5.

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
- [x] Convenciones VS Code: autocierre de paréntesis/comillas (con salto
  sobre el cierre y borrado en pareja), Enter dentro de `{}` abre bloque,
  `Tab`/`Shift+Tab` y `Ctrl+]`/`Ctrl+[` indentan líneas, `Ctrl+/`
  comenta, `Alt+↑/↓` mueve, `Shift+Alt+↑/↓` duplica, `Ctrl+Shift+K`
  borra, `Ctrl+L` selecciona la línea, `Ctrl+Delete`, línea actual
  resaltada, rueda de 3 líneas; el clic sólo alcanza paneles visibles
  (antes un panel oculto con bounds viejos «robaba» el clic al editor).
- [ ] Wrap opcional y scroll horizontal.
- [ ] «Recuperar N archivos» al arrancar para archivos que no se vuelven a
  abrir (hoy sólo al abrirlos).

## 3.3 — Sintaxis (`forge-syntax`)

- [x] tree-sitter 0.27 con gramáticas empaquetadas (rust, toml, json,
  bash, python, javascript, markdown, c) y sus `highlights.scm`; detección
  por extensión y shebang.
- [x] Parseo incremental en un hilo por buffer: el buffer registra cada
  cambio aplicado (`AppliedEdit`), el editor lo traduce a `InputEdit`, la
  UI lo aplica a su copia del árbol para que el resaltado siga al texto y
  el worker entrega árboles nuevos (peticiones coalescidas) que la UI
  adopta en el bucle de 16 ms; nunca se parsea en el hilo de UI.
- [x] Resaltado del rango visible con una sola consulta cacheada hasta el
  siguiente cambio (captura más interna gana), mapeado a 13 tokens del
  tema (`[syntax]` en temas), runs de color en `shape_line` con tabs
  expandidos.
- [ ] Gramáticas `.so` cargadas con `dlopen` desde `<config>/grammars/`,
  inyecciones (Markdown → código), folds e indents desde queries, caché de
  capturas por línea invalidada por `changed_ranges`.

## 3.4 — Proyecto y búsqueda (`forge-project`, `forge-search`)

- [x] `forge-project`: walker `ignore` (gitignore/.ignore/ocultos +
  exclusiones globales de §13.6, sin exigir repositorio git), índice de
  rutas en memoria con fuzzy `nucleo-matcher` (smart case, índices de
  coincidencia) y `FileWatcher` (`notify`) por archivo abierto.
- [x] `forge-search`: regex sobre el rope (buffer) y búsqueda de proyecto
  con `grep-searcher`/`grep-regex` en hilos, resultados en stream con
  tope (1.000) y cancelación al soltar el handle; literal/regex,
  mayúsculas y palabra completa.
- [x] GUI: «Ir a archivo» (`Ctrl+P`, índice en hilo de fondo, TTL 30 s),
  «Buscar en el proyecto» (`Ctrl+Shift+F` fuera de terminales, resultados
  en vivo, Enter abre en `ruta:línea:col`), barra de buscar/reemplazar en
  el editor (`Ctrl+F`/`Ctrl+H`: resaltado de coincidencias, Enter/Shift+Enter,
  Ctrl+Enter reemplaza, Ctrl+Alt+Enter todos, Alt+R/C/W), y recarga
  automática de buffers limpios que cambian en disco (aviso si están
  sucios o se borran).
- [ ] Explorador de archivos en árbol (panel lateral), índice `fst` para
  workspaces de 1M rutas, búsqueda por chunks con `regex-automata` para
  buffers muy grandes, reemplazo en proyecto.

## 3.5 — Archivos grandes, autosave y cierre

- [x] Modo grande (≥ 200 MB o línea > 1 MB): `mmap` de solo lectura,
  índice de líneas por chunks en un hilo, sin tree-sitter,
  `editor.materialize` carga en memoria (rechazado si supera la mitad de
  la RAM disponible) y reactiva edición/journal/resaltado (< 20 MB).
- [x] Autosave (`editor.autosave_ms`, 0 = off) desde el tick de
  housekeeping; keymap modal Helix-like básico (`editor.modal`: Normal
  con `i a o O hjkl w b 0 $ G x d u U v y p /` y `Esc`, INSERT con `Esc`
  para volver; el modo se ve en la línea de estado).
- [x] Benchmarks en `forge-bench` (`editor_typing`, `open_large_log`,
  `fuzzy_1m_paths`, `search_project_literal`) con umbrales en
  `bench/thresholds*.toml`; resultados DEV-1 en
  [`2026-09-14-editor.md`](../bench/results/2026-09-14-editor.md): tecleo
  2,7 ms mediana / 3,8 ms p99 (objetivo 3 ms p99, pendiente afinar),
  apertura de 256 MiB en 124 ms incluyendo el arranque, fuzzy 1 M rutas
  16 ms (objetivo 10 ms), búsqueda literal del repo 5 ms.
- [ ] Scroll 60/120 fps medido; RSS con el árbol de `linux` abierto
  ≤ 250 MB; `vtebench`-style comparación con rg en R1.
- [ ] Aceptación: editar Forge con Forge (dogfooding del usuario);
  `kill -9` recupera buffers vía journal al reabrir el archivo (✓ probado
  en tests; falta el diálogo «Recuperar N archivos» al arrancar); suite
  proptest verde (✓).
