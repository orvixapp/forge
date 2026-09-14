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
- [ ] Ejecutar 200 sesiones en R1 y validar `termd` ≤ 100 MiB.
- [ ] Ejecutar `vtebench` contra Alacritty y alcanzar ≥0,8× en R1.
- [ ] Medir latencia de input durante `cat` de 1 GiB (≤16 ms).

Evidencia: [`2026-09-13-key-echo.md`](../bench/results/2026-09-13-key-echo.md)
y [`2026-09-13-termd-idle-50.md`](../bench/results/2026-09-13-termd-idle-50.md).

## Resto de Fase 2

- [ ] `VtEngine` desacoplado y comparación Ghostty/Alacritty.
- [ ] ConPTY y transporte equivalente en Windows.
- [ ] Renderer completo: atlas, wide chars, emoji, estilos y redraw incremental.
- [ ] Entrada xterm/Kitty, mouse, selección, copiar/pegar y protección.
- [ ] Búsqueda de scrollback y navegación de resultados.
- [ ] OSC 7/8/52/133, integración de shell e hipervínculos `file:line`.
- [ ] Acciones finales de tabs/splits, renombrado, settings y señales.
- [ ] Conformance (`vttest`/`esctest`), benchmarks finales y CI verde.
