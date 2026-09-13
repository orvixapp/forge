# Fase 1 — Ejecución del shell

**Estado:** cierre en curso (2026-09-13). Este documento es el registro
operativo de Fase 1; no reemplaza [`ARCHITECTURE.md`](ARCHITECTURE.md), que
sigue siendo la fuente de decisiones y criterios de producto.

## Criterio de aceptación (ARCHITECTURE.md §29, Fase 1)

> se puede abrir, dividir paneles, cambiar tema y keymap, reiniciar y
> recuperar el layout; benchmarks verdes en CI en las 3 plataformas (Windows
> puede ser "build only").

| Criterio | Estado | Evidencia |
|---|---|---|
| Abrir, tabs, dividir paneles | ✅ | `Ctrl+T`, `Ctrl+W`, `Ctrl+\` / `Ctrl+Shift+5`, `Ctrl+Tab`; `PaneTree` con tests (`shell.rs`) |
| Cambiar keymap | ✅ | `[[keybindings]]` en `config.toml`, recarga sin reiniciar; tests de precedencia |
| Cambiar tema | ✅ | `forge-dark`/`forge-light` integrados, `themes/<n>.toml`, `[colors]`, `theme.cycle` (`Ctrl+Shift+T`); el tema activo se persiste en la sesión |
| Reiniciar y recuperar layout | ✅ | `session.json` versionado, guardado atómico solo si cambia, validado al cargar; un archivo corrupto se ignora con notificación |
| Config con capas y schema | ✅ | usuario → workspace (`.forge/config.toml`, sin `shell`/`args`), `docs/config.schema.json` generado con `--print-config-schema` |
| Palette y comandos | ✅ | 11 comandos con id estable, fuzzy, ↑/↓/Enter, atajo visible por fila |
| Diálogos nativos y notificaciones | ✅ | `terminal.newTabInDirectory` (selector nativo), confirmación al cerrar la última pestaña, notificaciones con caducidad en la barra |
| IME básico | ✅ (código) · ⏳ prueba manual | `EntityInputHandler`: commits multicarácter y preedit visible en la línea de estado; teclas muertas llegan como `key_char` |
| Process explorer vacío | ✅ | `processExplorer.show`: PID, PSS, frames, tema, fuente, sesiones |
| Tracing + Perfetto | ✅ | `FORGE_LOG=<filtro>`; `FORGE_TRACE_FILE=x.json` escribe traza Chrome (Perfetto la abre) con spans de arranque |
| CI en 3 plataformas | ⏳ ver último run | `cargo fmt/clippy -D warnings/test` en Linux, macOS, Windows (Windows: build only, sin daemon) |
| Benchmarks verdes en CI | ⏳ ver último run | Job `bench` (Xvfb + lavapipe) con `thresholds.ci.toml` |
| DPI / multi-monitor / Wayland+X11 | ⏳ prueba manual | Lista abajo |

## Benchmarks (portátil de referencia, ver `bench/results/2026-09-13-grid-full.md`)

| Escenario | Objetivo (§29) | Medido | Nota |
|---|---|---|---|
| `startup_empty` | ≤ 100 ms | 119–125 ms mediana | ~75 ms son inicialización de GPUI antes de nuestro código: 21 ms parseando 442 fuentes (cosmic-text), ~55 ms creando la instancia/dispositivo Vulkan (carga todos los ICD del sistema). Config + sesión: 0,1 ms. Ver traza con `FORGE_TRACE_FILE` y `FORGE_LOG=debug` |
| `idle` 60 s | 0 frames | 0 frames | Sondeo de config/sesión cada segundo sin `notify` salvo cambios |
| `panes_20` | ≤ 2 ms | 2,05 ms mediana · 2,67 ms p95 | 1 panel = 1,0 ms (suelo de draw+present de GPUI); 20 paneles añaden ~1 ms. Con flex anidado eran 21 ms: los paneles usan geometría explícita (`PaneTree::layout`) |
| RSS | ≤ 60 MB | 31–58 MB PSS | 20 paneles vacíos: 58 MB |
| `grid_full` (Fase 0) | ≤ 4 ms | 3,5 ms mediana · 4,7 p95 | Sin cambios en Fase 1 |

Los umbrales de producto (`bench/thresholds.toml`) se evalúan en las máquinas
de `bench/MACHINES.md`; CI usa `bench/thresholds.ci.toml`, calibrado para
runners sin GPU, y solo detecta regresiones de orden de magnitud.

## Pruebas manuales pendientes (registrar resultado y commit)

Cada prueba anota compositor, sesión (Wayland/X11), escala, tamaño y commit.

1. **Resize**: ampliar y reducir la ventana diez veces con una terminal
   activa (`yes | head -2000` antes). Esperado: sin corrupción, el PTY
   recibe un solo resize al soltar (60 ms de coalescencia).
2. **X11**: `WAYLAND_DISPLAY= cargo run --release` (o sesión X11). Esperado:
   igual que Wayland; bordes de resize y cursor correctos.
3. **DPI fraccional**: escala 125 % / 150 % en la pantalla; el texto se
   rasteriza nítido (subpixel variants) y las celdas no se solapan.
4. **Multi-monitor**: arrastrar la ventana entre pantallas con escalas
   distintas; la sesión guarda el tamaño lógico y se restaura.
5. **IME**: con `ibus`/`fcitx` y un método CJK, escribir "こんにちは" en la
   shell; el preedit aparece en la línea de estado y el commit llega a la
   shell. Teclas muertas (´ + e → é) sin IME.
6. **Sesión**: abrir tres pestañas y un split, cerrar Forge, reabrir:
   mismo layout, tamaño y tema. Corromper `session.json` a mano: Forge abre
   con una pestaña y avisa.
7. **Config**: editar `config.toml` con Forge abierto (fuente, tema,
   atajo): se aplica en ≤ 1 s; un TOML inválido deja la config anterior y
   notifica.

## Registro por subfase

### 1.0 — Ventana estable
- [x] `app_id`, assets de escritorio Linux, decoraciones cliente con bordes
  de resize (ocho asas con cursor propio, sin re-render por movimiento).
- [x] Tamaño mínimo 480×320, doble clic maximiza, clic derecho abre el menú
  nativo de ventana, botones minimizar/maximizar/cerrar.
- [x] Coalescencia de resize hacia el PTY (60 ms).
- [ ] Pruebas manuales 1–4.

### 1.1 — Núcleo de shell
- [x] Registro de comandos con ids estables y títulos; keymap con contextos
  `window`/`terminal`, precedencia determinista y overrides de usuario.
- [x] Todos los comandos del registro ejecutan desde el estado de shell.

### 1.2 — Layout
- [x] `PaneTree` serializable, splits H/V anidados, cierre de panel que
  colapsa el split, foco circular, geometría explícita con hueco de 2 px.
- [x] Barra de pestañas: activar, crear, cerrar (botón en la activa).

### 1.3 — Persistencia
- [x] `WindowSession` v2 (layout, cwd, tamaño, tema) con validación
  estructural, guardado atómico e idempotente, restauración al abrir.
- [x] Sesión corrupta → notificación y arranque limpio.

### 1.4 — Configuración
- [x] TOML por capas, claves prohibidas para el workspace, schema JSON,
  recarga en caliente, temas integrados y de usuario.
- [x] Decisión §22.1 registrada en ARCHITECTURE.md.

### 1.5 — UX
- [x] Palette fuzzy con atajos visibles, notificaciones con nivel y
  caducidad, selector nativo de directorio, confirmación nativa al cerrar.

### 1.6 — Cierre
- [x] Process explorer inicial, tracing/Perfetto, benchmark de 20 paneles,
  job de benchmarks en CI.
- [ ] CI verde en Linux/macOS/Windows en `main` (ver Actions).
- [ ] Pruebas manuales 5–7 y registro de la máquina en `bench/MACHINES.md`.

## Deuda conocida que pasa a Fase 2

- Windows: la GUI compila pero no hay daemon (ConPTY/named pipes: Fase 2).
- El IME de Wayland envía `done` sin preedit en cada cambio de foco; el
  manejador solo redibuja cuando cambia algo visible (verificado: `idle`
  sigue en 0 frames con el input handler registrado).
- Scrollback en la GUI: el protocolo no expone el viewport (Fase 2).
- El daemon reporta el cursor como no visible en prompts de p10k (Fase 2).
- `startup_empty` depende de GPUI (fuentes + Vulkan); revisar en R1 y
  considerar `VK_DRIVER_FILES` o carga perezosa de fuentes aguas arriba.

## Convenciones de implementación

- Cada subfase añade pruebas unitarias del estado puro y una prueba manual
  documentada para cualquier comportamiento dependiente del compositor.
- Las features de Fase 2 (scrollback, reflow de VT) no se usan para declarar
  terminado el layout de shell.
- Si una prueba visual falla, se conserva el artefacto y se anota el
  compositor, sesión (Wayland/X11), escala, tamaño inicial/final y commit.
