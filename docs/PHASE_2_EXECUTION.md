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
- [ ] Medir latencia de input durante `cat` de 1 GiB (≤16 ms).

Evidencia: [`2026-09-13-key-echo.md`](../bench/results/2026-09-13-key-echo.md)
y [`2026-09-13-termd-idle-50.md`](../bench/results/2026-09-13-termd-idle-50.md)
y [`2026-09-13-termd-idle-200.md`](../bench/results/2026-09-13-termd-idle-200.md).

## 2.3 — Compatibilidad del emulador

- [x] `VtEngine` desacopla lifecycle, IPC, PTY, snapshots, viewport e input
  del motor concreto; `GhosttyVtEngine` es la primera implementación.
- [x] Teclado xterm/Kitty codificado según los modos vivos del terminal.
- [x] Mouse, selección, copiar/pegar y bracketed paste atraviesan daemon y GUI.
- [x] Graphemes UTF-8 y emoji se conservan y se shapean en el renderer GPUI.
- [x] Cursor bar/block/underline/hollow y colores FG/BG llegan al renderer.
- [ ] Ampliar `ScreenCell` con bold, italic, faint, inverse, invisible,
  strikethrough, overline y variantes de underline; actualmente sólo conserva
  si la celda tiene estilo y sus colores resueltos.
- [ ] Conformance interactiva con `vim`, `htop`, `tmux`, `fzf`, Codex y Claude.
- [ ] Segunda implementación o adaptador de benchmark Alacritty para comparar
  detrás del mismo `VtEngine`.

## Resto de Fase 2

- [ ] Comparación Ghostty/Alacritty (`VtEngine` ya desacoplado).
- [ ] ConPTY y transporte equivalente en Windows.
- [ ] Renderer completo: atlas, wide chars, emoji, estilos y redraw incremental.
- [ ] Entrada xterm/Kitty, mouse, selección, copiar/pegar y protección.
- [ ] Búsqueda de scrollback y navegación de resultados.
- [ ] OSC 7/8/52/133, integración de shell e hipervínculos `file:line`.
- [ ] Acciones finales de tabs/splits, renombrado, settings y señales.
- [ ] Conformance (`vttest`/`esctest`), benchmarks finales y CI verde.
