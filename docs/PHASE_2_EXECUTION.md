# Ejecución de Fase 2 — Terminal

Documento vivo de implementación y validación. La fuente normativa de alcance
y presupuestos continúa siendo [`ARCHITECTURE.md`](ARCHITECTURE.md), §29.

## 2.1 — Daemon, persistencia y scrollback

- [x] Crear, listar y reatachar sesiones mantenidas por `forge-termd`.
- [x] Persistir el identificador de sesión por tab y reatachar al reiniciar la GUI.
- [x] Entregar un snapshot completo al cliente que se conecta tarde.
- [x] Exponer viewport y órdenes de scroll hasta inicio/final/delta/fila.
- [x] Conservar al menos 10.000 líneas: límite configurado a 100.000 líneas y
  64 MiB; ambos límites son necesarios porque Ghostty poda cuando alcanza el
  primero.
- [x] Prueba automatizada: desconectar el primer cliente, conectar otro,
  recuperar una sesión viva con 10.002 filas y navegar hasta la primera.
- [ ] Prueba de proceso: matar la GUI con `kill -9`, reiniciarla y comprobar
  visualmente pantalla, cursor y las 10.000 líneas.

## 2.2 — Rendimiento de la apuesta B

- [x] Tecla→eco por IPC/PTTY/VT: 0,14 ms mediana y 0,20–0,22 ms p95 en DEV-1.
- [x] Benchmark reproducible `termd_idle` con N sesiones.
- [x] 50 sesiones idle: 7.878 KiB PSS de `termd` (≈158 KiB/sesión).
- [x] Ejecutar 200 sesiones en DEV-1: 22.141 KiB PSS, dentro del límite de 100 MiB.
- [ ] Repetir la puerta de 200 sesiones en R1.
- [ ] Ejecutar `vtebench` contra Alacritty y alcanzar ≥0,8× en R1.
- [x] Medir latencia de input durante una salida de 1 GiB: 11,43 ms mediana,
  15,56 ms p95 en 50 muestras de DEV-1 (límite ≤16 ms).

Evidencia: [`2026-09-13-key-echo.md`](../bench/results/2026-09-13-key-echo.md)
y [`2026-09-13-termd-idle-50.md`](../bench/results/2026-09-13-termd-idle-50.md)
y [`2026-09-13-termd-idle-200.md`](../bench/results/2026-09-13-termd-idle-200.md)
y [`2026-09-13-flood-input.md`](../bench/results/2026-09-13-flood-input.md).

## 2.3 — Compatibilidad del emulador

- [x] `VtEngine` desacopla lifecycle, IPC, PTY, snapshots, viewport e input
  del motor concreto; `GhosttyVtEngine` es la primera implementación.
- [x] Teclado xterm/Kitty codificado según los modos vivos del terminal.
- [x] Mouse, selección, copiar/pegar y bracketed paste atraviesan daemon y GUI.
- [x] Graphemes UTF-8 y emoji se conservan y se shapean en el renderer GPUI.
- [x] Cursor bar/block/underline/hollow y colores FG/BG llegan al renderer.
- [x] `ScreenCell` transporta bold, italic, faint, blink, inverse, invisible,
  strikethrough, overline y variantes de underline desde Ghostty por IPC.
- [x] El renderer aplica bold/italic mediante variantes de fuente cacheadas,
  además de faint, inverse, invisible, underline simple/doble, strikethrough
  y overline.
- [x] Animar blink en intervalos de 500 ms; sólo solicitar frames mientras
  existen celdas parpadeantes para conservar el presupuesto idle.
- [ ] Conformance interactiva con `vim`, `htop`, `tmux`, `fzf`, Codex y Claude.
- [ ] Segunda implementación o adaptador de benchmark Alacritty para comparar
  detrás del mismo `VtEngine`.

## 2.4 — Búsqueda de scrollback

- [x] `Search`/`SearchResults` en el protocolo (v7): el daemon vuelca el área
  desplazable completa con el formatter de Ghostty (una línea por fila, sin
  tocar el render state ni el viewport) y compara en Rust (`search.rs`):
  literal o regex, sensible o no a mayúsculas, columnas en celdas (los
  caracteres anchos cuentan dos), tope de 10.000 coincidencias, error de
  patrón devuelto en la respuesta.
- [x] Barra de búsqueda (`terminal.search`, `Ctrl+Shift+F`): resaltado de
  todas las coincidencias visibles y de la actual, navegación con
  `Enter`/`Shift+Enter`/`↑`/`↓` y comandos `terminal.searchNext`/`searchPrevious`
  configurables, `Alt+R` regex y `Alt+C` mayúsculas; el viewport se centra
  en la coincidencia elegida (`Scroll::Row`).
- [x] Resultados estables: se descartan respuestas de peticiones antiguas
  (`request_id`), la búsqueda se repite (≤ 4 veces/s) al redimensionar o
  cuando llega salida nueva, y la selección se queda en la fila más cercana.
- [x] Prueba de integración `search_finds_scrollback_rows_and_reports_bad_patterns`:
  la fila de la coincidencia coincide con el espacio de filas del viewport.

## Resto de Fase 2

- [ ] Comparación Ghostty/Alacritty (`VtEngine` ya desacoplado).
- [ ] ConPTY y transporte equivalente en Windows.
- [ ] Renderer completo: atlas, wide chars, emoji, estilos y redraw incremental.
- [ ] Entrada xterm/Kitty, mouse, selección, copiar/pegar y protección.
- [ ] OSC 7/8/52/133, integración de shell e hipervínculos `file:line`.
- [ ] Acciones finales de tabs/splits, renombrado, settings y señales.
- [ ] Conformance (`vttest`/`esctest`), benchmarks finales y CI verde.
