# Fase 1 — Ejecución del shell

**Estado:** en curso. Este documento es el registro operativo de Fase 1; no
reemplaza [`ARCHITECTURE.md`](ARCHITECTURE.md), que sigue siendo la fuente de
decisiones y criterios de producto.

## Objetivo de cierre

Forge abre una shell estable que permite dividir paneles, cambiar de tab, usar
la palette y los keybindings, cambiar tema/keymap, reiniciar y recuperar el
layout. Los escenarios `startup_empty` e `idle_60s` quedan bajo umbral en CI.
No incorpora funcionalidades de editor ni convierte el prototipo de terminal
en la Fase 2.

## Subfases

| Subfase | Alcance | Evidencia de salida |
|---|---|---|
| 1.0 — Ventana estable | Chrome, icono, resize, foco, DPI/Wayland, minimizar/maximizar/cerrar. | Resize repetido sin corrupción ni cola de reflows; prueba manual Linux y tests de geometría. |
| 1.1 — Núcleo de shell | Estado de aplicación, registro de comandos y resolución de keymap. | Comandos testeados y acciones de shell no acopladas a widgets. |
| 1.2 — Layout | Tabs, splits H/V, paneles vacíos y navegación de foco. | Crear/dividir/cerrar/mover foco con teclado. |
| 1.3 — Persistencia | Layout, tabs, workspace y geometría de ventana. | Restauración segura tras reinicio; corrupción de estado no impide abrir Forge. |
| 1.4 — Configuración | JSONC, schema, capas usuario/workspace y recarga; temas/fuentes. | Cambios válidos se aplican sin reinicio y los inválidos preservan la configuración activa. |
| 1.5 — UX | Palette fuzzy, diálogos nativos, notificaciones y barra de estado. | Operaciones principales descubribles desde teclado. |
| 1.6 — Cierre | Process explorer inicial, trazas, benchmarks y CI. | `startup_empty`, `idle_60s` y builds de plataforma verdes. |

## Registro actual

### 1.0 — En curso

- [x] `app_id` y assets de escritorio Linux para asociación de taskbar.
- [x] Bordes/esquinas de decoraciones cliente inician el resize nativo.
- [x] Tamaño mínimo de ventana (480×320).
- [x] Doble clic en la barra de título para maximizar/restaurar y clic derecho
  para el menú nativo de ventana.
- [x] Coalescencia de cambios de tamaño: se descartan duplicados y se envía
  el último tamaño tras 60 ms de inactividad, para no reflowear el PTY por
  cada píxel del arrastre.
- [ ] Validación manual tras recompilar: Wayland y X11, ampliar/reducir diez
  veces con una terminal activa; registrar resultado y cualquier artefacto.
- [ ] Pruebas de DPI/multi-monitor.

### 1.1 — En curso

- [x] Registro de comandos con IDs estables y títulos para window, palette y layout.
- [x] Keymap con contextos `window`/`terminal`, precedencia determinista y pruebas.
- [x] `Ctrl+T` usa el registro para abrir una nueva ventana terminal.
- [ ] Ejecutar el resto de comandos desde el estado de shell; la palette y el
  árbol de layout se incorporan en 1.2 y 1.5.

### 1.2 — Núcleo completado; UI pendiente

- [x] Árbol serializable de panes, tabs y splits H/V.
- [x] Operaciones de dividir panel enfocado y recorrer el foco, con pruebas.
- [ ] Renderer de árbol, pestañas visibles y comandos de layout conectados a la ventana.

### 1.3 — Núcleo completado; restauración al arranque pendiente

- [x] Sesión versionada y guardado atómico de layout/tema.
- [x] Lectura tolerante a errores: una versión desconocida o JSON inválido no
  puede aplicarse como estado válido.
- [ ] Cargar al iniciar, guardar después de mutaciones y recuperar una sesión
  corrupta mostrando una notificación.

### 1.4 — Núcleo completado; aplicación visual pendiente

- [x] Loader JSONC por capas con precedencia defaults → usuario → workspace.
- [x] Comentarios JSONC, merge profundo y rechazo de capas inválidas.
- [x] `ShellSettings` inicial con tema, cubierto por prueba de precedencia.
- [ ] Schema publicado, watcher de recarga y aplicar tema/keymap activo sin reiniciar.

## Convenciones de implementación

- Cada subfase añade pruebas unitarias del estado puro y una prueba manual
  documentada para cualquier comportamiento dependiente del compositor.
- Las features de Fase 2 (scrollback, tabs de terminal, reflow de VT) no se
  usan para declarar terminado el layout de shell.
- Si una prueba visual falla, se conserva el artefacto y se anota el
  compositor, sesión (Wayland/X11), escala, tamaño inicial/final y commit.
