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
- [x] Segunda implementación detrás del mismo `VtEngine`: `alacritty_terminal`
  (`--features alacritty`, `--engine alacritty`) pasa la suite de integración
  completa; comparación en
  [`2026-09-13-engine-comparison.md`](../bench/results/2026-09-13-engine-comparison.md).

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

## 2.5 — OSC e integración de shell

- [x] OSC 7: `SessionInfo.pwd` (ya existía); las pestañas nuevas heredan el
  directorio y el título muestra su nombre.
- [x] OSC 8: cada `ScreenCell` transporta el URI (`hyperlink`), leído del
  render state sólo en filas que Ghostty marca con hipervínculos;
  `Ctrl+hover` subraya y `Ctrl+clic` abre en el navegador.
- [x] Detección de URLs y `ruta:línea:columna` en el texto (`links.rs`):
  se resuelven contra el cwd de la pestaña y se abren con
  `terminal.open_file_command` (plantilla `{file}`/`{line}`/`{column}`) o el
  abridor del escritorio; la Fase 3 los redirigirá al editor.
- [x] OSC 52 (y OSC 1337/5522 normalizados por Ghostty): el daemon acepta
  la escritura y la reenvía como `ClipboardWrite`; la GUI aplica
  `terminal.clipboard_write` (`ask` con diálogo y «permitir en esta
  pestaña», `allow`, `deny`). Las lecturas de portapapeles no se atienden.
- [x] OSC 133: `ScreenRow.prompt` desde el `semantic_prompt` de Ghostty,
  marca en el margen izquierdo de cada fila de prompt y navegación
  `terminal.previousPrompt`/`nextPrompt` (`ScrollToPrompt` en el daemon,
  escaneo acotado a 20.000 filas).
- [x] Scripts de integración para bash (`--rcfile` que carga los rc del
  usuario), zsh (`ZDOTDIR` puente que restaura el original) y fish
  (`vendor_conf.d` vía `XDG_DATA_DIRS`), inyectados sólo sin `terminal.args`
  y con `terminal.shell_integration = true`; `TERM_PROGRAM=forge`.
- [x] Protección de pegado: varias líneas sin bracketed paste o la
  secuencia `ESC [201~` piden confirmación (regla de `ghostty_paste_is_safe`
  consciente del modo 2004, que viaja en `SessionInfo.bracketed_paste`).
- [x] Prueba de integración `osc_marks_hyperlinks_and_clipboard_cross_the_daemon_boundary`.
- [ ] Copiar la salida del último comando: Ghostty no expone los límites C/D
  por fila en la C API; queda para cuando lo haga (o se reconstruya desde
  las marcas A).

## 2.6 — Acciones terminales

- [x] Renombrar pestañas (`terminal.renameTab`, `Ctrl+Shift+R`) con prompt
  en ventana; el nombre manda sobre OSC 7/título y se persiste en
  `session.json` (`titles`).
- [x] Mover/reordenar pestañas (`Ctrl+Shift+PageUp`/`PageDown`); los paneles
  del árbol de splits siguen a su pestaña (`PaneTree::swap_indices`).
- [x] Zoom de fuente `view.zoomIn`/`zoomOut`/`zoomReset` (`Ctrl+=`/`-`/`0`),
  pasos del 10 % sobre `font.size`, métricas de celda recalculadas y PTY
  redimensionado; el nivel se persiste.
- [x] Señales `terminal.signal.interrupt`/`terminate`/`kill` al grupo de
  procesos de la shell (`ClientMessage::Signal`, `rustix::kill_process_group`).
- [x] Splits finales: `layout.focusPreviousPane` (`Ctrl+Shift+Tab`),
  `layout.zoomPane` (`Ctrl+Shift+Enter`, sólo el panel activo) y
  `layout.unsplit`.
- [x] Perfiles por terminal (`[[profiles]]`: `name`, `shell`, `args`, `cwd`,
  `env`) con selector `terminal.newTabWithProfile`; la capa de workspace no
  puede definirlos (misma regla que `terminal.shell`).
- [x] Cierre limpio: cerrar pestaña → `ShutdownSession`; cerrar ventana deja
  las sesiones vivas en el daemon para reatachar.

## 2.7 — Plataformas y cierre

- [x] Windows: `proto-termd` escucha en un named pipe (`\\.\pipe\forge-prototype`,
  una instancia nueva por cliente) y usa ConPTY a través de `portable-pty`;
  la GUI conecta con el mismo nombre y reintenta mientras el pipe está
  ocupado. Las señales se degradan (Ctrl+C como byte; el resto termina el
  proceso). Verificado con `cargo check/clippy --target x86_64-pc-windows-gnu`
  para el daemon; la GUI no se puede cross-compilar aquí (`ring` necesita
  un toolchain MinGW), la valida el job de Windows de CI cuando se suba.
- [ ] Windows: ejecutar `forge-gui` + daemon en una máquina Windows real y
  construir `ghostty-vt.dll` (`scripts/bootstrap-ghostty.sh` es Unix).
- [x] Adaptador Alacritty (`VtEngine`), benchmark relativo y pruebas con
  ambos motores (`FORGE_TEST_ENGINE=alacritty`).
- [x] Procedimiento de conformidad `vttest`/`esctest` en
  [`CONFORMANCE.md`](CONFORMANCE.md); el registro se rellena en el bloque de
  validación manual.
- [x] Benchmarks finales de la fase en DEV-1 registrados
  ([`2026-09-13-engine-comparison.md`](../bench/results/2026-09-13-engine-comparison.md));
  los de la GUI de esa sesión son orientativos por carga de fondo y los
  valores de referencia siguen siendo los de `2026-09-13-grid-full.md`.
- [ ] CI: el pipeline queda como estaba (por decisión del usuario no se
  gasta cuota); al retomar, el job de Windows debe compilar `forge-gui` y el
  de Linux añadir `--features alacritty` a los tests del daemon.

## Pendiente para cerrar la fase (validación manual, bloque 1)

- [ ] Matar la GUI con `kill -9`, reabrir y comprobar pantalla, cursor y
  10.000 líneas (2.1).
- [ ] `vim`, `htop`, `tmux`, `fzf`, Codex y Claude sin glitches (2.3).
- [ ] Puerta de 200 sesiones y `vtebench` ≥ 0,8× Alacritty en R1 (2.2).
- [ ] `vttest`/`esctest` según `CONFORMANCE.md` y registrar el resultado.
- [ ] Windows real: daemon + GUI con ConPTY.
- [ ] Dos semanas de uso diario del equipo (criterio de aceptación §29).

Protocolo IPC: versión 7 (`Search`/`SearchResults` con error inline,
`ClipboardWrite`, `Signal`, `ScrollToPrompt`, `env` en `CreateSession`,
`hyperlink` por celda, `prompt` por fila, `bracketed_paste` en
`SessionInfo`).
