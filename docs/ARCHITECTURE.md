# Forge — Architecture & Engineering Plan

**Versión:** 0.2 (borrador para revisión) · **Fecha:** 2026-09-13 · **Autor:** arquitectura (Claude, sesión de diseño) · **Estado:** prototipo de Fase 0 en curso (`bench/results/`); decisiones, hipótesis y plan de validación. 0.2 añade la tesis humano + agente (§1, §17.7–§17.9).

> Filosofía: **el usuario controla el entorno, los procesos se aíslan y nada se carga hasta que realmente se necesita.**

## Cómo leer este documento

Cada decisión lleva una etiqueta:

| Etiqueta | Significado |
|---|---|
| **[DECIDIDO]** | Hay razones suficientes para comprometerse ya. Cambiarlo más tarde es caro. |
| **[RECOMENDADO → prototipo]** | Hay una recomendación clara, pero se valida en Fase 0/1 antes de construir encima. |
| **[ABIERTO]** | No hay que decidirlo todavía; decidirlo ahora sería adivinar. Ver la sección final. |
| **[VERIFICAR]** | Afirmación sobre un tercero tomada de una fuente no contrastada (p. ej. una conversación con un modelo); comprobar contra su documentación antes de construir sobre ella. |

Los números de rendimiento son **objetivos con método de medición**, no promesas. Los baselines reales de VS Code, Zed, Neovim, Helix, Alacritty, Kitty y Ghostty se miden en Fase 0 en las máquinas de referencia (§5.1) y se corrigen los objetivos si hace falta.

## 1. Resumen ejecutivo

Forge es un entorno de desarrollo **terminal-first** para trabajo asistido por agentes: un emulador de terminal de primera clase, un editor de código rápido, integración nativa con agentes (Claude Code, Codex, OpenCode, Gemini CLI y cualquiera que hable ACP), soporte LSP y Git, y una capa de compatibilidad progresiva con extensiones de VS Code. No es un fork de VS Code ni usa Electron.

**Tesis del producto:** Forge no es "un editor con IA" sino un **entorno de desarrollo para humano + agente**: el mejor sitio para trabajar con agentes de programación sin hacerle la vida incómoda al humano. Humano y agente ven **el mismo workspace, el mismo estado y las mismas herramientas** (archivos, buffers sin guardar, terminal, git, LSP, búsqueda); el humano conserva teclado, terminal, atajos y personalización; el agente recibe contexto y herramientas sin copiar y pegar. Forge **no compite con Codex ni con Claude Code**: les da el entorno. Se detalla en §17.7–§17.9.

**Decisiones centrales (resumen):**

1. **Un lenguaje de sistema: Rust** para todo lo que es nuestro (UI, editor, terminal, IPC, supervisor). **JavaScript/Node.js** solo para ejecutar extensiones VS Code, porque esas extensiones *son* Node. No hay Go ni C++ en el núcleo. **[DECIDIDO]**
2. **Renderer GPU con GPUI** (el framework de Zed, Apache-2.0) como apuesta principal, con un *spike* de 2 semanas que la confirma o la sustituye por `winit + wgpu + cosmic-text + taffy` (que es, en la práctica, lo que construiríamos si GPUI falla). El renderer de "grid de texto" (editor y terminal) se diseña como módulo propio independiente del framework de chrome. **[RECOMENDADO → prototipo]**
3. **No inventar un protocolo de agentes.** Adoptar **ACP (Agent Client Protocol)** como contrato: ya lo soportan Claude Code, Codex, OpenCode, Gemini CLI, Copilot CLI y 25+ agentes. La abstracción `Agent` propuesta en el brief es incompleta porque le falta la mitad del protocolo: **el agente llama al editor** (leer/escribir buffers, crear terminales, pedir permisos). **[DECIDIDO]**
4. **MCP** en dos direcciones: Forge es *cliente* MCP (configura servidores y los pasa a los agentes por ACP) y *servidor* MCP (expone buffers, diagnósticos, símbolos, git y terminales a cualquier agente). **[DECIDIDO]**
5. **Procesos:** un proceso principal (`forge`: ventana + renderer + core + supervisor), un daemon de terminales (`forge-termd`) que sobrevive a la UI, N extension hosts Node, y los procesos externos que ya son procesos por naturaleza (LSP, agentes, MCP). Ni "supervisor como proceso aparte" ni "renderer y core separados" de entrada: se diseña la frontera, no se paga el coste. **[RECOMENDADO → prototipo]**
6. **Terminal sobre `alacritty_terminal` + `portable-pty`**, con integración de shell (OSC 133/7/8), scrollback por niveles y persistencia de sesión vía daemon. Se evalúa `libghostty-vt` cuando su API se estabilice. **[RECOMENDADO → prototipo]**
7. **Editor sobre rope (`ropey`/`crop`) + tree-sitter + cliente LSP propio**, ediciones como operaciones (no como "reemplazar buffer") para que parsing incremental, LSP incremental, undo y ediciones de agentes compartan el mismo primitivo. Sin CRDT. **[DECIDIDO]**
8. **Extensiones VS Code en Node.js real** (bundled, versión fijada), con nuestra propia implementación TypeScript del namespace `vscode` guiada por un **escáner de uso de API** sobre las 1.000 extensiones más instaladas de Open VSX. Sin QuickJS ni V8 embebido para esto. **[DECIDIDO]** El registro es **Open VSX** (los ToS del Marketplace de Microsoft lo prohíben). **[DECIDIDO]**
9. **Configuración como datos (TOML + JSON Schema generado), no como código.** Sin lenguaje de scripting en v1. Las automatizaciones se expresan como comandos y macros. **[DECIDIDO en Fase 1]** (§22.1)
10. **Rendimiento como restricción continua, no como fase.** La suite de benchmarks es un crate del workspace desde la Fase 1 y corre en CI con umbrales. La "Fase 10: optimización extrema" del brief se elimina. **[DECIDIDO]**
11. **Modelos intercambiables sin cliente LLM propio.** Las suscripciones existentes (ChatGPT → Codex, Anthropic → Claude Code), los gateways compatibles con OpenAI (p. ej. Token Harbor) y los modelos locales (llama.cpp) son **perfiles de proveedor** que Forge inyecta en la configuración de los agentes, y un **enrutador por clase de tarea** (trivial / normal / compleja / sin red) decide qué agente y qué perfil atienden cada petición. Forge no llama a APIs de modelos por sí mismo. **[RECOMENDADO → Fase 4]**

**Coste honesto:** un equipo de 2–4 ingenieros senior con agentes de programación tarda del orden de **12–15 meses** en llegar a la Fase 8 (extension host con extensiones de lenguaje reales funcionando). La compatibilidad VS Code es una cola abierta que nunca llega al 100% y que hay que dimensionar con datos (§20.3), no con optimismo.

### 1.1 Críticas al brief (leer antes que el resto)

El brief es bueno. Estas son las partes que no aceptaría tal cual:

| # | Idea del brief | Problema | Qué se propone |
|---|---|---|---|
| 1 | `Supervisor` como par del `Renderer/Core` | El proceso que posee la ventana recibe el input del SO; si el supervisor es otro proceso, cada evento cruza IPC y el supervisor no puede "supervisar" al proceso del que depende para vivir. Modelo Chrome (browser process minúsculo + renderers) vale para *muchas* ventanas heterogéneas; para un IDE es coste sin beneficio claro. | El supervisor es un **módulo** dentro de `forge`. Lo que merece sobrevivir a un crash de la UI son las terminales (`forge-termd`) y los buffers (journal en disco). Se deja la frontera diseñada para poder separar core/UI en el futuro ("headless core"). |
| 2 | Abstracción `Agent { start, send, interrupt, events, capabilities, shutdown }` | Es solo la dirección editor→agente. Lo que hace útil un agente en un IDE es la dirección **agente→editor**: leer el buffer sin guardar, escribir con revisión, crear terminales, pedir permiso. Además, ya existe un estándar con adopción real (ACP). | Adoptar ACP como contrato; la abstracción interna es un adaptador fino (§17). |
| 3 | "Fase 10: Optimización extrema" | Las fases finales de optimización no ocurren: el proyecto llega con deuda estructural imposible de pagar (asignaciones por frame, protocolos verbosos, estados globales). | Presupuestos de rendimiento y benchmarks en CI desde la Fase 1 (§28). |
| 4 | `Rust → core, Go → servicios, JS → extensiones` | Dos toolchains, dos sistemas de build, dos modelos de memoria, doble superficie IPC, y Go no aporta nada que Rust+tokio no haga para "gestión de procesos". El único lenguaje adicional que *tiene* razón arquitectónica es JS, y no por elección: las extensiones VS Code son JS. | Rust + TypeScript (solo en el ext host). §8. |
| 5 | "Miles de terminales/procesos" | El límite no es el número de PTYs (el kernel aguanta miles) sino la **memoria de scrollback**: 10k líneas × 200 columnas × ~8 B/celda ≈ 16 MB por terminal en el peor caso. 1.000 terminales así = 16 GB. | Diseñar para *cientos activas / miles existentes*: scrollback por niveles (caliente en memoria compacta, frío comprimido, spill a disco), límites por terminal y globales (§14.5). |
| 6 | "Suspensión de procesos inactivos" | `SIGSTOP` a un LSP con peticiones pendientes bloquea al cliente; Windows no tiene suspensión documentada; macOS no tiene límites de memoria por proceso. Valor marginal. | Apagar LSPs/ext hosts inactivos tras un timeout largo y rearrancar bajo demanda; priorizar (nice/QoS) en vez de suspender (§10.4). |
| 7 | "GPU cuando sea posible" | Sin política definida, "cuando sea posible" termina en tickets de soporte (Zed en Linux es el ejemplo). | GPU obligatoria vía wgpu (Vulkan/Metal/DX12) **con** soporte probado en CI del driver software de Mesa (llvmpipe) como modo degradado. No hay renderer CPU propio (§12.6). |
| 8 | Compatibilidad VS Code "progresiva" | Sin una herramienta que mida qué APIs usa el ecosistema real, "progresiva" es una lista de deseos. Además hay límites **legales**: Marketplace ToS, Pylance, C/C++, Remote-*, Copilot, Live Share no funcionarán nunca fuera de VS Code. | Escáner de uso de API en Fase 0 que produce la tabla de cobertura real; Open VSX; tiers (§20). |
| 9 | "Personalizar prácticamente todo" | El camino habitual (Lua/JS en la config) crea un segundo ecosistema de plugins, incompatible con el primero, y convierte la config en código no auditable que corre con privilegios del usuario. | Config declarativa con schema, comandos componibles (macros), contextos `when`. Scripting es **[ABIERTO]** para después (§22). |
| 10 | Orden de fases (editor → terminal → LSP → git → agentes) | Para un producto *terminal-first*, la terminal + agentes es el MVP que se puede usar a diario (los agentes son TUIs). El editor puede ser mediocre unas semanas; la terminal no. | Terminal en Fase 2, agentes en Fase 4 (antes de LSP). §29. |

---

## 2. Objetivos

| Objetivo | Medida de éxito |
|---|---|
| Terminal de primera clase, usable como emulador diario desde el primer mes | Dogfooding del equipo; paridad de vtebench con Alacritty ±20%; latencia de input <16 ms bajo flood |
| Editor rápido en repos grandes | Abrir el kernel de Linux (~80k archivos) y editar con latencia de tecleo indistinguible de un archivo pequeño; archivo de 1 GB visible en <500 ms |
| Agentes como ciudadanos de primera clase, desacoplados | Cualquier agente ACP funciona sin código específico; Claude Code, Codex, OpenCode y Gemini CLI probados en CI |
| Humano y agente comparten estado y herramientas | Toda herramienta del humano (búsqueda, LSP, git, terminal, tareas) es un comando expuesto al agente por MCP; el agente ve buffers sin guardar; la traza de tool calls es visible y navegable; "investigar este error" llega al agente con archivo, línea, diagnóstico, diff y salida de terminal sin copiar nada |
| Aprovechar suscripciones y modelos baratos | Codex con la cuenta de ChatGPT del usuario sin configuración extra; una tarea trivial enrutada a un modelo gratuito/local **no consume cuota** de la suscripción; el usuario ve qué ruta atendió cada petición |
| Compatibilidad VS Code medible y creciente | Dashboard público de cobertura de API; top-50 extensiones de lenguaje funcionando en Fase 7 |
| RAM baja y **predecible** | Presupuestos por componente con límites duros (§24); "process explorer" integrado |
| Personalización profunda sin caos | Todo es un comando; todo comando es enlazable; toda opción tiene schema y aparece en el buscador de settings |
| Aislamiento de fallos | Un crash de extensión, LSP o agente nunca cierra la UI ni pierde una terminal |
| Multiplataforma | Linux (Wayland/X11) y macOS desde Fase 1; Windows desde Fase 3 con CI, paridad completa en Fase 8 |

## 3. Non-goals

Explícitamente **fuera** de alcance para v1 (algunos para siempre):

- **Fork de VS Code o de Zed.** Se reutilizan crates con licencia permisiva (GPUI es Apache-2.0); los crates del editor de Zed son GPL-3.0 y no se copian.
- **Colaboración en tiempo real / CRDT.** Es la razón por la que la arquitectura de Zed es tan compleja. Las ediciones se modelan como operaciones (lo cual no lo impide en el futuro) pero no hay relojes vectoriales ni sincronización.
- **Notebooks (Jupyter).** La API de notebooks de VS Code es enorme y la usan pocas extensiones de forma proporcional a su coste.
- **Desarrollo remoto** (SSH/contenedores como *workspace*). v1 solo ejecuta `ssh` en una terminal. La arquitectura deja la puerta abierta (§31).
- **Versión web o móvil.**
- **Marketplace propio.** Open VSX + instalación desde VSIX local.
- **Lenguaje de scripting propio en la configuración** (v1).
- **Emulación Vim completa** en v1. Sí: modos y keymaps modales (Helix-style) como mecanismo; la emulación Vim exhaustiva es un paquete posterior.
- **Framework GUI de propósito general.** Forge no es una librería; si GPUI no sirve, la alternativa es un renderer mínimo específico, no "nuestro propio GPUI".
- **Renderer CPU propio.** Solo GPU vía wgpu + driver software de Mesa como degradado.
- **Accesibilidad completa en v1.** No es aceptable ignorarla arquitectónicamente (se elige framework con camino a AccessKit) pero no bloquea el roadmap hasta Fase 8.
- **Telemetría remota.** Métricas locales, opt-in, exportables por el usuario.
- **Un agente propio que compita con Codex o Claude Code.** Forge orquesta agentes y les da el entorno; un agente interno mínimo solo se consideraría si un proveedor no tuviera ningún agente ACP, y como último recurso **[ABIERTO]**.
- **Un cliente de APIs de LLM en el núcleo.** Los modelos se alcanzan a través de los agentes (§17.7).

---

## 4. Requisitos funcionales

Prioridad: **P0** = necesario para el MVP terminal-first (fases 1–4); **P1** = necesario antes de v1.0; **P2** = después.

### 4.1 Shell de aplicación
- P0: ventana(s) nativa(s), event loop, DPI/escala fraccional, multi-monitor, IME, portapapeles, drag & drop de archivos, diálogos nativos.
- P0: layout de paneles como **árbol de splits** (H/V) cuyas hojas son **grupos de tabs**; docks izquierda/derecha/abajo también son árboles; todo navegable por teclado; persistido por workspace.
- P0: sistema de **comandos** (nombre, args, contexto), **keybindings** con contextos `when`, chords y secuencias; **command palette** con fuzzy matching.
- P0: **temas** (colores + tokens semánticos), fuentes (familia/tamaño/ligaduras/fallback) por superficie (editor/terminal/UI).
- P1: perfiles, workspaces multi-root, ventanas múltiples, "process explorer" (RSS/CPU por proceso hijo), notificaciones, barra de estado extensible.

### 4.2 Terminal
- P0: PTY real (openpty/ConPTY), bash/zsh/fish/PowerShell, múltiples sesiones, splits/tabs, resize, señales (Ctrl-C/Z, SIGWINCH, SIGHUP al cerrar), procesos interactivos (vim, htop, agentes TUI), mouse (SGR 1006), scroll, selección (celda/palabra/línea/bloque), copy/paste (bracketed paste), 256/truecolor, atributos, cursor styles, búsqueda en scrollback, hyperlinks OSC 8, cwd OSC 7.
- P0: **integración de shell** OSC 133 (prompts semánticos): saltar entre comandos, seleccionar salida del último comando, estado de salida por comando, y —clave para agentes— "dame la salida del último comando" sin scraping.
- P0: sesiones **persistentes** ante crash/reinicio de la UI (daemon).
- P1: Kitty keyboard protocol, synchronized output (DEC 2026), OSC 52 (con permiso), scrollback por niveles con spill a disco, renombrado/color de tabs, `--command` para lanzar tareas, reflow al redimensionar.
- P2: gráficos Kitty/Sixel, SSH como *workspace* remoto, adjuntar a sesión desde otra instancia (multiplexing tipo tmux).

### 4.3 Editor
- P0: rope, multi-cursor, undo/redo transaccional, selección múltiple, búsqueda/reemplazo con regex, syntax highlighting por tree-sitter, indentación, folding, bracket matching, autoclose, soft-wrap, minimap opcional, gutter (números, git, diagnósticos), archivos grandes (modo degradado automático), encodings/EOL, autosave y journal.
- P0: explorador de archivos respetando `.gitignore` + exclusiones globales; fuzzy finder de archivos (nucleo); búsqueda en proyecto (motor de ripgrep).
- P1: keymaps modales (Helix-like), snippets, símbolos del workspace (índice tree-sitter incremental), inlay hints, semantic tokens, diff view, rename de archivos con notificación a LSP.
- P2: emulación Vim, code lens, breadcrumbs, sticky scroll.

### 4.4 LSP / Git / Agentes / MCP / Extensiones
- LSP P1: multi-servidor por lenguaje, completion/hover/definition/references/rename/diagnostics (push y pull)/format/code actions/signature help/inlay/semantic tokens/workspace symbols/watched files, cancelación y coalescing, registro de servidores por lenguaje, instalación asistida (P2).
- Git P1: status/diff en gutter, blame, log de archivo, staging por hunk, commit, branches, stash, conflictos con editor de 3 vías, **worktrees** (los agentes los usan).
- Agentes P0: sesiones ACP, prompt con contexto (selección, archivo, @-menciones), streaming de respuesta, tool calls visibles, **permisos** con opciones recordables, **ediciones propuestas** revisables hunk a hunk antes de tocar disco, terminales creadas por el agente en `forge-termd` y visibles al usuario, cancelación, resume (`session/load`), modos (plan/edit/auto), varios agentes en paralelo (por worktree). P1: perfiles de proveedor y enrutamiento por clase de tarea (§17.7), acciones contextuales `agent.ask`/`agent.investigate` desde selección, diagnóstico, terminal y diff (§17.8), línea de tiempo de herramientas (§17.9).
- MCP P1: config de servidores (stdio/HTTP) por usuario/workspace, passthrough a agentes por ACP, Forge como servidor MCP (herramientas: archivos abiertos, contenido de buffers, diagnósticos, símbolos, git status, ejecutar comando en terminal con permiso).
- Extensiones P1/P2: instalación desde Open VSX/VSIX, `contributes` declarativos (temas, gramáticas, snippets, comandos, configuración, keybindings, lenguajes), Node host, API `vscode` por tiers (§20), aislamiento, límites, reinicio.

---

## 5. Requisitos de rendimiento

### 5.1 Máquinas de referencia

Sin máquinas fijas los números no significan nada. Se definen tres perfiles y se documenta el hardware exacto en `bench/MACHINES.md`:

| Perfil | Hardware | Para qué |
|---|---|---|
| **R1 — portátil típico** | 8 núcleos x86-64 o Apple M-series, 16 GB, iGPU, NVMe, pantalla 2× | Objetivos por defecto |
| **R2 — estación** | 16+ núcleos, 32 GB, GPU discreta, 144 Hz | Techo de rendimiento; repos enormes |
| **R3 — modesto** | 4 núcleos, 8 GB, sin GPU (llvmpipe), VM o CI | Modo degradado; el producto debe seguir siendo usable |

Metodología general: N=10 ejecuciones, reportar **mediana y p95**, máquina sin otras cargas, gobernador de CPU fijo, misma versión de corpus. Corpus fijos con commit pinned: `linux` (~80k archivos), un monorepo TypeScript sintético (100k archivos, 10 GB `node_modules` para probar exclusiones), un `.log` de 1 GB, un JSON de 100 MB en una línea, un `.ts` de 10k líneas.

### 5.2 Objetivos

| Métrica | Objetivo R1 | Cómo se mide | Referencia informal (verificar en Fase 0) |
|---|---|---|---|
| Startup a primer frame (caliente) | **≤ 100 ms** | `forge --exit-after-first-frame` + `hyperfine`; span `startup` en tracing | Zed ~100–200 ms; VS Code 1–3 s; Neovim ~30 ms; Alacritty ~50 ms |
| Startup (frío, tras drop caches) | ≤ 300 ms | igual, con `echo 3 > drop_caches` | — |
| RSS workspace vacío (proceso `forge` + `termd`) | **≤ 80 MB**, techo 120 MB | PSS vía `/proc/*/smaps_rollup` (justo con libs compartidas); `ps` en macOS | Zed 150–300 MB; VS Code 300–600 MB; Helix 30–60 MB; Alacritty 30–60 MB (el driver GPU pesa 30–50 MB por sí solo) |
| RSS con `linux` abierto (sin LSP) | ≤ 250 MB en core; índice de 1 M rutas ≤ 50 MB | idem, tras indexado completo; `fst` para rutas | — |
| Latencia tecla→present (interna) | **p99 ≤ 3 ms** desde evento OS hasta llamada `present` | timestamps en el pipeline; exportable como trace | Objetivo externo: aparecer en el **siguiente frame** (≤16.7 ms @60 Hz), medido con Typometer |
| Frame time durante scroll continuo | ≤ 4 ms p95 (deja margen a 144 Hz) | contador de frames + GPU timestamps de wgpu | — |
| Frames en reposo | **0** (event-driven; solo parpadeo de cursor, desactivable) | contador de frames en 60 s idle | Muchos editores GPU redibujan siempre |
| CPU idle (3 terminales + 1 LSP cargado) | ≤ 0,5 % promedio en 60 s | `getrusage`/`/proc/stat` diff | — |
| Abrir archivo de 1 GB | visible ≤ 500 ms; RSS extra ≤ 64 MB (mmap + índice de líneas lazy) | span + PSS | VS Code rechaza >~50 MB con highlighting; Neovim/Helix cargan en RAM |
| Editar archivo de 100 MB con highlighting | latencia de tecleo igual que archivo pequeño (±1 ms) | mismo pipeline | — |
| Búsqueda literal en `linux` | ≤ 1,2× ripgrep | `rg --stats` vs búsqueda integrada | Usamos los crates de ripgrep; la diferencia es el transporte de resultados |
| Fuzzy finder sobre 1 M rutas | ≤ 10 ms p95 por tecla | bench de `nucleo` con nuestro índice | Helix usa nucleo con estos órdenes |
| Autocomplete (overhead de Forge) | ≤ 1 ms desde respuesta LSP a popup; tiempo total reportado por separado | timestamps en cliente LSP | El servidor domina; medimos *nuestro* overhead |
| Terminal: `cat` 1 GB | ≥ 0,8× Alacritty en vtebench; input latency ≤ 16 ms durante el flood | vtebench + inyección de tecla durante flood | Kitty/Ghostty/Alacritty son la referencia |
| Terminal: RSS por terminal idle | ≤ 2 MB con scrollback de 10k líneas típicas; techo configurable 32 MB | PSS de `termd` / N | — |
| Indexado de símbolos de `linux` | ≤ 10 s en 8 núcleos; incremental ≤ 50 ms por archivo guardado | span + contador | — |
| Ext host arranque (Node, sin extensiones) | ≤ 150 ms; RSS ≤ 50 MB | span + PSS | Node baseline ~40 MB |
| Crash → recuperación de buffers | 100 % del contenido no guardado hasta la última operación (journal fsync ≤ 1 s) | test de kill -9 | — |

Presupuestos de RAM por componente en §24.


---

## 6. Arquitectura propuesta

### 6.1 Principios

1. **Protocolos en las fronteras de proceso, APIs dentro.** Todo lo que cruza un proceso es un mensaje versionado con handshake de capacidades (el patrón de LSP/ACP/MCP). Dentro de `forge`, son traits y canales de Rust.
2. **Servicios con transparencia de ubicación.** Terminal, indexador, git y cliente LSP se escriben contra un `Transport` (canal in-process o socket). Se decide *por medición* si corren en hilo o en proceso. Excepción deliberada: buffer del editor ↔ renderer es in-process y comparte memoria; es la ruta caliente.
3. **Todo es un comando.** Cada acción del usuario, cada acción de extensión, cada acción de agente es `Command { id, args }` en un registro único. Keybindings, palette, macros, menús y la API MCP de Forge son vistas de ese registro.
4. **Activación por manifiesto.** Nada se carga sin un evento de activación: lenguaje abierto, comando invocado, `workspaceContains`, vista visible. Vale para extensiones VS Code, para gramáticas, para LSPs y para agentes.
5. **Ediciones como operaciones.** `Edit { range, replacement }` con versión de buffer es el primitivo compartido por undo, tree-sitter (`ts_tree_edit`), LSP (`didChange` incremental), ediciones propuestas por agentes y diff review.
6. **Presupuestos, no esperanzas.** Cada componente tiene presupuesto de RAM y de tiempo por frame; se mide en CI (§24, §28).
7. **Fallar aislado.** Un panic en un subsistema se captura en su frontera; un crash de un hijo se reinicia con backoff; el estado que importa (buffers, terminales) sobrevive por diseño.

### 6.2 Vista de alto nivel

```text
┌──────────────────────────────────────────────────────────────────────────────┐
│ forge  (proceso principal — posee la ventana)                                │
│                                                                              │
│  ┌──────────────┐   ┌───────────────────────────┐   ┌─────────────────────┐  │
│  │ UI / Renderer│◄──┤ Core                      │◄──┤ Supervisor (módulo) │  │
│  │ GPUI/wgpu    │   │ · Buffers (rope)          │   │ · spawn/monitor     │  │
│  │ · layout tree│   │ · Commands + Keymap       │   │ · heartbeats        │  │
│  │ · text system│   │ · Config (capas+schema)   │   │ · límites/cgroups   │  │
│  │ · grid render│   │ · Workspace/Project index │   │ · restart backoff   │  │
│  └──────────────┘   │ · Syntax (tree-sitter)    │   └─────────┬───────────┘  │
│                     │ · LSP client · Git (gix)  │             │              │
│                     │ · Agent client (ACP)      │             │              │
│                     │ · MCP client + server     │             │              │
│                     │ · Extension API (nativa)  │             │              │
│                     └───────────────────────────┘             │              │
└───────────────────────────────────────────────────────────────┼──────────────┘
                                                                │ IPC (unix socket / named pipe)
        ┌───────────────┬───────────────┬──────────────┬────────┴───────┬───────────────┐
        ▼               ▼               ▼              ▼                ▼               ▼
 ┌─────────────┐ ┌─────────────┐ ┌────────────┐ ┌────────────┐ ┌──────────────┐ ┌───────────┐
 │ forge-termd │ │forge-exthost│ │ LSP server │ │ ACP agent  │ │ MCP server   │ │ git (CLI) │
 │ PTYs + VT   │ │ Node.js     │ │ (externo)  │ │ codex /    │ │ (externo,    │ │ mutaciones│
 │ persistente │ │ vscode API  │ │ ×N         │ │ claude /   │ │ stdio/http)  │ │ efímero   │
 │ ×1          │ │ ×N grupos   │ │            │ │ opencode.. │ │ ×N           │ │           │
 └─────────────┘ └─────────────┘ └────────────┘ └────────────┘ └──────────────┘ └───────────┘
   protocolo        JSON-RPC        JSON-RPC       JSON-RPC        JSON-RPC       porcelain -z
   binario propio   (ext host)      (LSP)          (ACP)           (MCP)
```

Diferencias respecto al diagrama del brief: (a) el supervisor es un módulo del proceso que posee la ventana; (b) no hay "Agent host" como proceso: los agentes ya son procesos y hablan ACP directamente con el core; (c) la terminal sí es un proceso aparte porque es el único estado que vale la pena que sobreviva a la UI; (d) "Background workers" es un pool de hilos dentro de `forge` hasta que un benchmark demuestre que un worker concreto (p. ej. indexador) debe salir a proceso.

---

## 7. Alternativas consideradas

| Alternativa | Por qué se descarta (o se pospone) |
|---|---|
| **Fork de VS Code / VSCodium** | Descartado por mandato. Además: Electron, ~100 MB RSS mínimo por ventana, arquitectura de workbench pensada para el DOM. |
| **Shell webview (Tauri/wry) con UI en HTML** | Más ligero que Electron pero conserva el coste de un motor de navegador para *cada* pixel; la latencia de input y el idle CPU los dicta el motor. Solo se usa wry **para webviews de extensiones** (§20.5). |
| **Fork de Zed** | Tiene editor+terminal+LSP+ACP hoy. Pero: crates del editor bajo GPL-3.0 (implicaciones de licencia para el producto), arquitectura atravesada por CRDT/colaboración, sin compatibilidad VS Code, y heredar ~1 M LOC ajenas no es "no reinventar", es "mantener lo que otro diseñó". Se reutiliza **GPUI** (Apache-2.0) y se estudian sus diseños. |
| **GUI para Neovim embebido** (modelo Neovide/VimR: nvim como motor por msgpack-RPC) | Atajo enorme para el editor (madurez, plugins). Pero fija un modelo modal Vim-céntrico, config en Lua, grid sin UI proporcional, y la compatibilidad VS Code sería un injerto. Si el producto fuera "la mejor GUI de Neovim para agentes" sería la elección correcta; no es ese producto. |
| **Helix-core como librería** | MPL-2.0, crates `helix-core` (ropey + tree-sitter + LSP types) reutilizables selectivamente. **Se adopta parcialmente**: nucleo, ideas de `languages.toml`, textobjects por tree-sitter. No se adopta su UI (TUI). |
| **Qt / GTK como toolkit principal** | Madurez y accesibilidad reales, pero C++ (o bindings con fricción), LGPL/comercial en Qt, GTK débil fuera de Linux, y el contenido (grid de texto) igual necesita un renderer GPU propio. Se usan **diálogos nativos** (`rfd`) y nada más. |
| **Shell nativo por plataforma + core compartido** (modelo Ghostty: SwiftUI + GTK) | La mejor sensación nativa, al precio de N interfaces de usuario. Para un equipo pequeño, no. Para un IDE (VS Code, Zed, JetBrains no son nativos) la expectativa de "nativo" es baja. |
| **Renderer y core en procesos separados desde el día 1** | Neovim lo hace bien, pero paga con un protocolo de UI limitado (grid). Con UI rica, todo el estado de layout cruzaría IPC. Se diseña la frontera (core sin dependencias de GPUI) y se separa **solo** si aparece el caso de uso (remote/headless). |
| **Extension host en QuickJS / V8 embebido / deno_core** | §21. |
| **CRDT para buffers** | §13.7. |
| **libgit2 (`git2`)** | §16. |

---

## 8. Decisiones tecnológicas

### 8.1 Lenguaje

| Criterio | Rust | Go | C++ | Zig |
|---|---|---|---|---|
| Rendimiento / control de memoria | Excelente; sin GC; control de layout | Bueno; GC con pausas cortas pero presentes en el hilo del renderer | Excelente | Excelente |
| Seguridad de memoria | Sí (salvo `unsafe` acotado) | Sí (GC) | No | Parcial |
| Ecosistema GUI/GPU | **wgpu, GPUI, winit, cosmic-text, vello** — el mejor de los cuatro para este dominio | Prácticamente nulo (Fyne, Gio no son de nivel IDE); CGo penaliza | Skia, Qt, Dear ImGui; excelente pero heterogéneo | Inexistente salvo Ghostty (que hace su propio renderer por plataforma) |
| Piezas de dominio reutilizables | `alacritty_terminal`, `portable-pty`, `ropey`/`crop`, tree-sitter, `lsp-types`, `gix`, `ignore`/`grep` (ripgrep), `nucleo`, `notify`, `keyring`, `deno_core` | Pocas para editor/terminal | libghostty-vt, tree-sitter, libgit2 | libghostty |
| Concurrencia | tokio + rayon; ownership evita data races | Goroutines (ideal para servicios) | Manual | Manual |
| Interop con C | Excelente (bindgen/cxx) | CGo (lento, complica builds) | Nativo | Excelente |
| Tooling / build | cargo, un solo comando; compilación lenta | go build; muy rápido | CMake y compañía; doloroso | zig build; inmaduro |
| Riesgo de proyecto | Tiempos de compilación; curva de aprendizaje | Tener que escribir el renderer GPU desde cero | Bugs de memoria en un producto con extensiones no confiables | Ecosistema pre-1.0 |

**Decisión [DECIDIDO]:** Rust para todo el código propio. TypeScript para el shim `vscode` (corre en Node, es inevitable). Nada de Go: "servicios y gestión de procesos" en Rust con tokio es igual de cómodo y evita un segundo runtime, un segundo build y una frontera IPC extra. Se acepta el coste conocido: tiempos de compilación (mitigación: workspace con muchos crates pequeños, `cargo check` en el bucle interno, `mold`/`lld`, cache de CI).

Zig merece una nota: Ghostty demuestra que se puede construir la mejor terminal en Zig, y `libghostty-vt` es consumible desde Rust. Se puede adoptar *como librería*, sin escribir Zig.

### 8.2 Tabla de decisiones

| Área | Decisión | Confianza | Validación |
|---|---|---|---|
| Lenguaje | Rust (+ TS en ext host) | Alta | — |
| Framework UI | GPUI (crate `gpui`; considerar `gpui-ce` si la API upstream rompe demasiado) | **Media** | Spike Fase 0 (§31) |
| Fallback UI | winit + wgpu + cosmic-text + taffy + capa retained mínima propia | Media | Solo si el spike falla |
| Texto | cosmic-text (rustybuzz + swash + fontdb) o el text system de GPUI; atlas de glifos propio para el grid | Media | Spike Fase 0 |
| Buffer | `ropey` (madurez) con evaluación de `crop`; SumTree propio solo si se necesita metadata por nodo | Alta | Bench Fase 3 |
| Sintaxis | tree-sitter + queries (highlights/injections/folds/indents/textobjects, convenciones Helix/nvim) | Alta | — |
| Terminal VT | `alacritty_terminal` | Alta | Comparar con `libghostty-vt` en Fase 2 |
| PTY | `portable-pty` (openpty + ConPTY) | Alta | — |
| LSP | cliente propio sobre `lsp-types` + `tower-lsp`-style codec | Alta | — |
| Git | `gix` para lecturas; `git` CLI para mutaciones; **no** libgit2 | Alta | — |
| Búsqueda | crates `ignore`, `grep-searcher`, `grep-regex` (ripgrep) | Alta | — |
| Fuzzy | `nucleo` | Alta | — |
| Watch | `notify` + debouncer; fallback polling en subárboles | Alta | — |
| Agentes | ACP (JSON-RPC sobre stdio) | Alta | Spike Fase 0 con claude-code-acp y codex |
| MCP | crate `rmcp` (SDK oficial Rust) para cliente y servidor | Alta | — |
| Ext host | Node.js LTS bundled; shim `vscode` propio en TS | Alta | Escáner Fase 0 |
| Webviews | `wry` como vista hija superpuesta | **Baja** | Spike Fase 8 |
| IPC | unix socket / named pipe; framing propio; MessagePack interno, JSON-RPC externo | Alta | Bench termd Fase 0 |
| Config | TOML + JSON Schema generado, capas usuario/workspace | **Alta** | Decidido en Fase 1 (§22.1) |
| Allocator | mimalloc | Alta | — |
| Observabilidad | `tracing` + exportación Perfetto/Tracy; `minidumper` para crashes | Alta | — |
| Async | tokio (I/O), rayon (CPU), hilo de UI dedicado | Alta | — |

---

## 9. Diagrama de componentes (dentro de `forge`)

```text
                         ┌──────────────────────────────┐
                         │          ui (GPUI)           │
                         │  workspace · panes · docks   │
                         │  palette · settings ui       │
                         │  editor_view · terminal_view │
                         │  agent_view · diff_view      │
                         └──────────────┬───────────────┘
                                        │ traits + canales (in-process)
┌───────────────────────────────────────┼─────────────────────────────────────────────┐
│ core                                  ▼                                             │
│ ┌────────────┐ ┌──────────────┐ ┌──────────────┐ ┌───────────────┐ ┌──────────────┐ │
│ │ commands   │ │ keymap       │ │ config       │ │ theme/fonts   │ │ workspace    │ │
│ │ registry   │ │ contexts     │ │ layers+schema│ │               │ │ layout state │ │
│ └─────┬──────┘ └──────────────┘ └──────────────┘ └───────────────┘ └──────────────┘ │
│       │                                                                             │
│ ┌─────▼──────┐ ┌──────────────┐ ┌──────────────┐ ┌───────────────┐ ┌──────────────┐ │
│ │ buffer     │ │ syntax       │ │ project      │ │ search        │ │ journal      │ │
│ │ rope+edits │ │ tree-sitter  │ │ walk+index   │ │ ripgrep crates│ │ autosave     │ │
│ └────────────┘ └──────────────┘ └──────────────┘ └───────────────┘ └──────────────┘ │
│                                                                                     │
│ ┌────────────┐ ┌──────────────┐ ┌──────────────┐ ┌───────────────┐ ┌──────────────┐ │
│ │ lsp_client │ │ git          │ │ agent (ACP)  │ │ mcp           │ │ ext_api      │ │
│ │ multi-srv  │ │ gix + cli    │ │ sessions     │ │ client+server │ │ providers    │ │
│ └────────────┘ └──────────────┘ └──────────────┘ └───────────────┘ └──────────────┘ │
│                                                                                     │
│ ┌────────────┐ ┌──────────────┐ ┌──────────────┐ ┌───────────────┐                  │
│ │ terminal   │ │ supervisor   │ │ ipc          │ │ permissions   │                  │
│ │ client     │ │ lifecycle    │ │ transport    │ │ policy+audit  │                  │
│ └────────────┘ └──────────────┘ └──────────────┘ └───────────────┘                  │
└─────────────────────────────────────────────────────────────────────────────────────┘
```

Regla de dependencias: `ui → core`, nunca al revés. `core` no depende de GPUI. Esto es lo que permite (a) tests headless de todo el core, (b) un futuro core remoto, (c) cambiar el framework UI si el spike de GPUI falla sin tocar el 70 % del código.

Layout de crates propuesto (workspace Cargo):

```text
crates/
  forge-app/         binario principal; solo composición
  forge-ui/          GPUI: vistas, layout, temas
  forge-text/        shaping, atlas, grid renderer (independiente de GPUI en lo posible)
  forge-core/        commands, keymap, config, workspace state
  forge-buffer/      rope, edits, undo, journal
  forge-syntax/      tree-sitter, queries, highlight cache
  forge-project/     walker, ignore, índice de rutas (fst), watcher
  forge-search/      ripgrep crates, resultados en stream
  forge-lsp/         cliente LSP
  forge-git/         gix + cli
  forge-term/        cliente de termd + tipos de protocolo
  forge-termd/       binario daemon: PTY + VT + scrollback
  forge-agent/       cliente ACP, sesiones, permisos, ediciones propuestas
  forge-mcp/         cliente + servidor MCP
  forge-ext/         API nativa de extensiones (providers) + supervisor de ext hosts
  forge-ipc/         framing, codec, transportes, heartbeat
  forge-supervisor/  lifecycle de hijos, límites, restart
  forge-bench/       harness de benchmarks + escenarios
ext-host/            TypeScript: shim `vscode`, loader, RPC (se compila a un bundle)
tools/
  vscode-api-scan/   escáner de uso de API sobre Open VSX
```


---

## 10. Diagrama de procesos y ciclo de vida

### 10.1 Procesos

| Proceso | Cardinalidad | Vive cuánto | Por qué es un proceso |
|---|---|---|---|
| `forge` | 1 por instancia (varias ventanas en el mismo proceso) | sesión del usuario | Posee la ventana y el GPU device |
| `forge-termd` | 1 por usuario (compartido entre instancias) | hasta que no queden terminales o el usuario lo pare | **Persistencia**: los jobs del usuario sobreviven a crashes/actualizaciones de la UI; base para multiplexing y attach remoto |
| `forge-exthost` | 1 por grupo de extensiones (por defecto: uno para todas las de confianza; opcional 1:1 para pesadas/no confiables) | mientras haya extensiones activas; se apaga tras N min de inactividad si no tiene providers registrados | Aislamiento: JS no bloquea la UI por construcción; límites de memoria; reinicio |
| LSP servers | 1 por (servidor, raíz de workspace) | mientras haya buffers de ese lenguaje; apagado tras timeout largo | Son procesos por definición del protocolo |
| Agentes ACP | 1 por sesión (o por agente, si soporta multi-sesión) | durante la sesión | Idem |
| MCP servers (stdio) | 1 por servidor configurado, bajo demanda | mientras algún consumidor los use | Idem |
| `git` | efímero por operación de mutación | ms | Compatibilidad total (hooks, credenciales, LFS) |

Hijos de `forge` reciben `PR_SET_PDEATHSIG(SIGTERM)` en Linux / Job Object con `KILL_ON_JOB_CLOSE` en Windows, **salvo `forge-termd`**, que se desacopla (setsid) deliberadamente.

### 10.2 Máquina de estados de un hijo

```text
   spawn()        handshake ok         idle > T_idle
Spawning ──────► Ready ──────► Running ◄────────────► Idle
   │ timeout       │ hb miss×3 / exit≠0                 │ T_shutdown
   ▼               ▼                                    ▼
 Failed ◄──── Crashed ──restart(backoff)──► Spawning   Stopped
   ▲               │ > 3 restarts / 60 s
   └───────────────┘
```

- **Handshake**: `initialize { protocol_version, capabilities }` → `initialized`. Igual que LSP/ACP/MCP; un hijo que no responde en 10 s (configurable, LSPs pesados necesitan más) se marca `Failed` con diagnóstico en UI.
- **Heartbeat** solo para hijos propios (`termd`, `exthost`): ping cada 5 s, 3 fallos → "colgado" → `SIGTERM`, 5 s de gracia, `SIGKILL`. LSP/agentes/MCP no implementan heartbeat: se usa liveness (`waitpid`) + timeouts por petición + detección de estancamiento (peticiones pendientes sin ninguna respuesta en 60 s → aviso con botón "reiniciar").
- **Restart**: backoff exponencial 1 s → 2 → 4 → 8, máximo 3 reinicios por 60 s; después `Failed` hasta acción del usuario. Al reiniciar un ext host se re-ejecutan los eventos de activación que estaban vivos; al reiniciar un LSP se re-envían `didOpen` de los buffers abiertos.
- **Shutdown ordenado**: `shutdown` request → `exit` notification → esperar 3 s → `SIGKILL`. `termd` en cambio recibe `detach`, no `shutdown`, salvo que el usuario cierre "todo".

### 10.3 Límites de recursos

| Plataforma | Mecanismo | Nota |
|---|---|---|
| Linux | cgroup v2 por hijo (`memory.max`, `memory.high`, `cpu.weight`) cuando el usuario tiene delegación (systemd user slice: `systemd-run --user --scope`); si no, fallback a monitor de RSS | `memory.high` produce throttling antes del OOM: preferible |
| Windows | Job Object: `JOB_OBJECT_LIMIT_PROCESS_MEMORY`, `JOB_OBJECT_LIMIT_JOB_MEMORY`, prioridad | Bien soportado |
| macOS | **No hay límite duro fiable por proceso.** Monitor de RSS (cada 2 s vía `proc_pidinfo`) + política kill/restart; para Node `--max-old-space-size` limita el heap V8 | Aceptar la limitación y decirlo en la doc |

Política por defecto: ext host 512 MB (soft) / 768 MB (hard); LSP sin límite duro (rust-analyzer legítimamente usa GBs) pero con aviso configurable; agentes sin límite (son del usuario); `termd` limitado por presupuesto de scrollback (§14.5), no por cgroup.

### 10.4 Prioridad y "suspensión"

En vez de `SIGSTOP`: `nice`/`SCHED_IDLE` en Linux, `SetPriorityClass(BELOW_NORMAL)` en Windows, QoS `background` en macOS para indexador y ext hosts sin foco. Los LSPs se **apagan** tras `lsp.idle_shutdown = 30m` sin buffers abiertos de su lenguaje (configurable; `never` para rust-analyzer si el usuario quiere). Suspender con señales queda como experimento **[ABIERTO]** solo para Linux y solo cuando no hay peticiones en vuelo.

### 10.5 Backpressure

- Todos los canales son **acotados**. Tamaño por canal documentado en el código.
- Streams (salida de terminal, resultados de búsqueda, diagnósticos masivos, tokens de agentes) usan **control de flujo por créditos**: el consumidor concede N unidades; el productor no envía más hasta recibir más créditos. La terminal es el caso claro: si la UI no drena, `termd` deja de leer el PTY y el kernel bloquea al proceso hijo (comportamiento correcto y gratuito).
- La UI **coalesce**: entre dos frames, solo importa el último estado del grid; el parser corre a toda velocidad, el render ocurre a frame rate.
- Respuestas grandes de LSP (completion con 10k ítems) se truncan en el cliente con `isIncomplete = true`.

---

## 11. IPC

### 11.1 Transporte

| Mecanismo | Veredicto |
|---|---|
| **Unix domain socket** (Linux/macOS) / **named pipe** (Windows) | **[DECIDIDO]** para todos los hijos propios. Latencia ~10–50 µs por mensaje, throughput de GB/s, permisos por filesystem (0600), sin puertos. Abstracción: `tokio::net::UnixStream` / `tokio::net::windows::named_pipe`. |
| stdio | Para hijos externos que lo exigen (LSP, ACP, MCP stdio). Mismo framing que el protocolo defina. |
| TCP localhost | **Descartado** para lo propio: cualquier proceso local puede conectarse, cortafuegos, colisión de puertos. Solo cliente de MCP Streamable HTTP cuando el servidor es HTTP. |
| Shared memory | **[ABIERTO]** para rutas calientes *si* la medición lo justifica. Candidatos: grids de terminal (`termd → forge`) y vistas de archivos gigantes. Cálculo: un grid de 50×200 celdas ≈ 80 KB; a 60 fps son 4,8 MB/s por terminal muy activa — un socket lo hace sin despeinarse. Empezar por mensajes; añadir shm solo con bench. |
| D-Bus/XPC/COM | No. |

### 11.2 Framing y codificación

```text
frame := u32 length (LE) | u8 kind | u8 flags | u16 reserved | payload
kind  := Request | Response | Notification | StreamItem | Credit | Cancel
```

- **Interno** (`forge ↔ termd`, `forge ↔ exthost` control): payload **MessagePack** (`rmp-serde`) con tipos Rust compartidos (`forge-ipc` crate; el lado TS usa `@msgpack/msgpack`). Flag `JSON` para modo depuración legible. Razón: JSON-RPC para filas de terminal es ~3× más bytes y base64; los formatos zero-copy (rkyv/FlatBuffers) no compensan su fricción de esquema hasta que un perfil diga lo contrario.
- **Externo**: lo que dicte el protocolo. LSP: `Content-Length` headers + JSON. ACP/MCP stdio: JSON-RPC 2.0 con JSONL. No se "mejoran".
- Versionado: `initialize` intercambia `protocol_version` (semver) y `capabilities`; los campos nuevos son opcionales; nunca se reutiliza un `kind`.
- Mensajes >1 MB: `flags.OUT_OF_BAND` con referencia a un archivo temporal (o shm en el futuro). Evita que una respuesta gigante bloquee el socket.
- Cancelación: `Cancel { request_id }`; todo handler recibe un `CancellationToken`.

### 11.3 Latencia objetivo

Round-trip `forge ↔ termd` para un keystroke (tecla → write al PTY → eco → fila sucia → frame): **≤ 0,5 ms** de overhead IPC en R1. Se mide en el prototipo (§ Primer prototipo). Si supera 1 ms de forma consistente, el diseño de termd como proceso separado se revisa (feature flag para correr in-process con el mismo código, gracias a §6.1 principio 2).

---

## 12. Renderer

### 12.1 Comparativa

| Opción | Rendimiento | RAM | Multiplataforma | Madurez | Texto | Personalización | Mantenimiento | Riesgo |
|---|---|---|---|---|---|---|---|---|
| **GPUI** (Zed) | Excelente; diseñado para un editor | Media (atlas, escena retenida) | mac/Linux oficial; Windows funciona en Zed 1.15 pero el crate lo declara "mac o Linux" | Pre-1.0, **breaking changes frecuentes** | Propio (CoreText / DirectWrite / cosmic-text) con caché de shaping; probado a escala | Alta (elementos tipo Tailwind, layout flex) | Seguir a Zed; fork `gpui-ce` existe como red de seguridad | API churn; Windows |
| winit + wgpu + cosmic-text + taffy (custom) | Excelente | Baja | Sí | Piezas maduras; **el framework lo escribimos nosotros** | cosmic-text maduro; atlas propio | Total | Alto: toda la capa retained, focus, hit-testing, scroll, IME, accesibilidad | Tiempo: Zed tardó años en GPUI |
| egui | Bueno | Baja | Sí | Madura | Aceptable; sin subpixel; sin sistema de texto de nivel editor | Media (immediate mode limita) | Bajo | Inadecuado para tipografía de editor de primer nivel; redibujo total |
| Iced | Bueno | Baja | Sí | Media | cosmic-text | Media (Elm) | Medio | Menos probado a escala de IDE |
| Slint | Bueno | Baja | Sí | Media | Propio | DSL propio | Medio | Licencia GPL/comercial/royalty-free con restricciones |
| Makepad | Excelente | Baja | Sí | Baja | Propio (SDF) | Alta | Alto | Equipo pequeño |
| Vello + Parley + Xilem (Linebender) | Excelente (compute) | Media | Sí | **Alpha** (2026) | Parley prometedor | Alta | Medio | No apto para producción todavía; vigilar |
| Skia (`skia-safe`) | Excelente | Alta (binario ~20 MB+) | Sí | Muy madura | La mejor | Alta (pero es solo 2D; el framework sigue faltando) | Build C++ pesado | Peso; dependencia C++ |
| Qt / GTK | Bueno | Alta | Qt sí / GTK débil fuera de Linux | Muy madura | Buena | Media | Bindings | Licencia (Qt), sensación no-editor |
| WebGPU en webview | Bueno | Alta | Sí | Media | Motor del navegador | Alta | Bajo | Reintroduce el navegador |

### 12.2 Recomendación **[RECOMENDADO → prototipo]**

**GPUI**, con estas salvaguardas:

1. `forge-core` no depende de GPUI (§9). Solo `forge-ui` y `forge-app`.
2. `forge-text` (grid de editor/terminal: shaping cache, atlas, quads instanciados) se escribe contra wgpu directamente o contra la primitiva de "custom element" de GPUI, de modo que sobrevive a un cambio de framework.
3. Se fija la versión de `gpui` y se actualiza en ventanas planificadas (cada 2–3 meses), no en cada release de Zed. Si el coste de seguir upstream supera ~2 días/mes, se migra a `gpui-ce` (fork orientado a estabilidad) o se fija definitivamente.
4. Windows: el spike de Fase 0 incluye compilar en Windows. Si no funciona, Windows se retrasa (no se cambia de framework por Windows solo).

Cuantificación del fallback: una capa retained mínima (árbol de elementos, layout flex vía taffy, focus, hit-testing, scroll, clipping, IME, texto vía cosmic-text) son del orden de **3–4 meses** de un ingeniero senior antes de escribir el primer panel del editor. GPUI regala eso. El coste de GPUI es el churn: estimación **2–4 días/mes** de mantenimiento de versiones. Es una buena compra siempre que el spike demuestre que (a) el custom element para grids es viable, (b) el arranque y RSS cumplen §5.2, (c) Wayland con escala fraccional funciona.

### 12.3 Pipeline de frame

```text
evento OS ─► cola de input ─► actualización de estado (core) ─► layout (solo subárboles sucios)
          ─► display list (quads, glyph runs, paths, scissors) ─► encode wgpu (instanced) ─► present
```

- **Event-driven**: no hay loop de render continuo. Se redibuja cuando cambia el estado o llega un vsync solicitado (animaciones/scroll). En reposo: 0 frames (§5.2). El parpadeo del cursor es el único temporizador y es desactivable.
- **Dirty regions**: en la práctica significa tres cosas, por orden de impacto: (1) no dibujar frames cuando nada cambió; (2) cachear por panel: display list y líneas shaped se reutilizan si el panel no cambió; (3) redibujo parcial con damage rects — solo lo soportan bien algunos compositores (Wayland sí, Metal no de forma útil). Un frame completo de texto en GPU cuesta ~0,5–1 ms; (3) es una micro-optimización que **no** se hace en v1.
- **Virtualización**: solo las líneas visibles (+ margen) se shapean y rasterizan. El rope da acceso O(log n) por línea; tree-sitter se consulta por rango visible; LSP semantic tokens por rango (`textDocument/semanticTokens/range`).
- **Draw calls**: objetivo ≤ 10 por frame típico (un pipeline para quads/glifos, uno para paths, scissor por panel).

### 12.4 Texto

- Shaping: HarfBuzz vía `rustybuzz` (o el de GPUI). Caché de shaping por `(texto, fuente, tamaño, features)`; las líneas de terminal se cachean por fila.
- Rasterización: `swash` (portable) o rasterizador de plataforma (CoreText/DirectWrite) para consistencia con el sistema. **[ABIERTO]**: medir calidad y coste; GPUI ya elige por plataforma.
- Atlas: texturas 2048² R8 (grayscale) + RGBA (emoji/color), asignación con `etagere`, LRU; presupuesto ≤ 64 MB VRAM.
- Subpixel positioning horizontal (¼ píxel), hinting configurable, ligaduras configurables por superficie (mucha gente las quiere en el editor y no en la terminal), fallback de fuentes en cadena (usuario → sistema → Noto), emoji color (COLR/CBDT), celdas anchas CJK en terminal, `Nerd Fonts`.
- Bidi (UAX#9): **no** en v1 (limitación documentada). Se elige un sistema de texto que no lo impida.

### 12.5 Plataforma

wgpu: Vulkan (Linux), Metal (macOS), DX12 (Windows). Wayland con `fractional-scale-v1`, X11 con `Xft.dpi`, macOS con backing scale. IME vía GPUI/winit (`preedit` en la posición del cursor: imprescindible para CJK). Diálogos nativos con `rfd`. Accesibilidad: exposición del árbol vía AccessKit cuando GPUI lo soporte de forma completa (**[ABIERTO]**, seguimiento en roadmap Fase 8+).

### 12.6 Política de GPU

- Requisito: cualquier adaptador que wgpu acepte, **incluido** `llvmpipe`/`lavapipe` (Mesa) como software. CI corre la suite de render con lavapipe.
- Pérdida de device (driver reset, suspend): recrear device, surface, atlas y pipelines; el estado de la app no vive en la GPU.
- Sin GPU en absoluto (contenedor sin Vulkan): mensaje claro con instrucciones (`mesa-vulkan-drivers`), no un crash.


---

## 13. Editor

### 13.1 Estructura del buffer

| Estructura | Inserción/borrado | Acceso por línea/offset | Multi-cursor (N ediciones) | Notas |
|---|---|---|---|---|
| Vector de líneas | O(línea) | O(1) línea | O(N·línea) | Falla con líneas de 100 MB (JSON minificado) |
| Gap buffer | O(1) local, O(n) al saltar | O(n) línea | Malo | Emacs; no para multi-cursor |
| **Piece table** (VS Code) | O(log p) | O(log p) con árbol de piezas | Degenera con muchas ediciones dispersas (fragmentación de piezas) | Excelente para "cargar y leer", peor para sesiones largas |
| **Rope** (Zed, Helix, xi) | O(log n) | O(log n) | O(N log n) | Balanceado, inmutable-friendly (snapshots baratos para hilos de fondo), offsets en bytes que tree-sitter y LSP necesitan |

**[DECIDIDO] Rope.** `ropey` (madura, usada por Helix) por defecto; `crop` se evalúa en el bench de Fase 3 (afirma ser más rápido; menos probado). Un SumTree propio (como Zed) solo si hacen falta métricas adicionales por nodo (p. ej. altura de líneas envueltas para scroll proporcional) y el bench lo justifica.

Snapshots: el rope es persistente (clonar es O(1)); los hilos de fondo (tree-sitter, búsqueda, LSP sync, journal) trabajan sobre un snapshot con versión, nunca sobre el buffer vivo.

### 13.2 Ediciones y undo

- Primitivo: `Transaction { edits: Vec<Edit{range, text}>, selections_before, selections_after, version }`. Los edits de una transacción se aplican en orden inverso de posición (así los rangos no se invalidan entre sí).
- Undo: pila de transacciones con **inversas** calculadas al aplicar; agrupación por tiempo (ediciones a <300 ms se fusionan) y por tipo (tecleo contiguo). Undo tree (Vim/Helix) **[ABIERTO]**: barato de añadir si la pila es un árbol desde el principio; se implementa como árbol pero se expone como pila en v1.
- Cada transacción notifica a: tree-sitter (`InputEdit`), cliente LSP (`didChange` incremental con debounce), journal, gutters de git, y a la vista.

### 13.3 Sintaxis: tree-sitter

- Un parser por (buffer, lenguaje) + parsers de inyección (Markdown→código, HTML→JS/CSS, Rust→SQL en macros, etc.).
- Parsing incremental en hilo de fondo con snapshot; timeout de parse (tree-sitter soporta cancelación) para no bloquear en archivos patológicos; el resultado anterior sigue sirviendo para pintar.
- Queries: `highlights.scm`, `injections.scm`, `locals.scm`, `folds.scm`, `indents.scm`, `textobjects.scm` con las convenciones de Helix/nvim-treesitter (ecosistema de gramáticas más grande). Las gramáticas se compilan a `.so`/`.dylib`/`.dll` y se cargan **bajo demanda** con `dlopen` (o WASM vía `tree-sitter` wasm — más lento; solo para gramáticas no confiables **[ABIERTO]**).
- Highlighting: solo del rango visible ± 1 pantalla; caché de capturas por línea invalidada por rango cambiado del árbol (`changed_ranges`).
- Mapeo captura → token semántico del tema (`@keyword` → `keyword`). Compatibilidad con temas VS Code: tabla de mapeo capture → TextMate scope (§20.4).

### 13.4 Archivos grandes

Umbrales configurables, por defecto:

| Tamaño | Comportamiento |
|---|---|
| < 20 MB | Normal |
| 20–200 MB | Sin LSP; tree-sitter con timeout corto; sin minimap; wrap desactivado por defecto |
| > 200 MB o línea > 1 MB | **Modo grande**: lectura por `mmap`, índice de líneas construido lazy por chunks en fondo, sin tree-sitter, highlighting por regex simple opcional, solo lectura hasta la primera edición; a la primera edición se materializa el rope (con aviso si supera la RAM disponible / 2) |

### 13.5 Multi-cursor y selecciones

Selecciones como `Vec<Selection{anchor, head}>` ordenadas y fusionadas tras cada operación (modelo Helix: la selección es primaria, el cursor es una selección de longitud 0/1). Operaciones se expresan sobre *todas* las selecciones (map) para que multi-cursor no sea un caso especial.

### 13.6 Búsqueda e índice

- En buffer: `regex` crate sobre el rope (por chunks con `regex-automata` streaming para no copiar).
- En proyecto: crates de ripgrep (`ignore` para el walker paralelo con `.gitignore`/`.ignore` + exclusiones globales; `grep-searcher` + `grep-regex` para buscar). Resultados en stream con créditos; la UI muestra los primeros 1.000 y sigue contando.
- **Exclusiones globales por defecto** (indexado/búsqueda, **no** apertura): `.git`, `node_modules`, `target`, `vendor`, `build`, `dist`, `out`, `.cache`, `__pycache__`, `.venv`, `venv`, `.gradle`, `.idea`, `.next`, `.turbo`, `coverage`, `*.min.js`, `*.map`. Un archivo excluido sigue siendo abrible por ruta o por go-to-definition. Regla: nunca se escanea `node_modules` salvo petición explícita.
- Índice de rutas: `fst` (autómata finito comprimido) para 1 M rutas en decenas de MB; fuzzy con `nucleo` sobre el índice.
- Índice de símbolos del workspace: por tree-sitter con `tags.scm`, incremental por archivo, en SQLite o en un formato propio append-only **[ABIERTO]**; solo para archivos no excluidos; construido en fondo con prioridad baja y pausable.

### 13.7 Qué se implementa y qué se reutiliza

| Implementar | Reutilizar |
|---|---|
| Transacciones/undo, selecciones, comandos de edición, vistas, wrap, folding UI, journal, modo archivo grande, índice de símbolos | rope (`ropey`), tree-sitter + gramáticas, `regex`, ripgrep crates, `nucleo`, `fst`, `notify`, `lsp-types`, `encoding_rs`, `unicode-segmentation`/`unicode-width` |

**No CRDT.** Un CRDT resuelve edición concurrente entre réplicas; aquí hay una sola réplica de verdad (el core) y los agentes proponen ediciones contra una versión, que se rebasean o se rechazan (§17.5). Es un modelo mucho más simple y suficiente.

---

## 14. Terminal

### 14.1 Arquitectura

```text
forge (UI)                                   forge-termd (daemon, 1 por usuario)
┌────────────────────┐   unix socket / pipe   ┌──────────────────────────────────────┐
│ terminal_view      │◄──────────────────────►│ session manager                      │
│ · grid cache       │  Input{bytes,keys}     │ ├─ Terminal #1 ─ PTY fd ─ VT state    │
│ · glyph runs       │  Resize{cols,rows}     │ ├─ Terminal #2 ─ PTY fd ─ VT state    │
│ · selection        │  Damage{rows,cells}    │ └─ ...                               │
│ · search UI        │  Event{title,cwd,133}  │ · reader loop (tokio, N fds)          │
└────────────────────┘  Credit{n}             │ · parser pool (rayon)                 │
                                              │ · scrollback tiers                   │
                                              │ · shell integration                  │
                                              └──────────────────────────────────────┘
```

- **VT state machine**: `alacritty_terminal` **[RECOMENDADO]** (parser `vte` + grid + scrollback + selección + modos; Apache-2.0; Zed lo usa). Alternativas: `wezterm-term`/`termwiz` (más features: images, más pesado, acoplado a wezterm), `libghostty-vt` (Zig, C API, sin dependencias, muy rápido, API todavía inestable; bindings Rust existen). Se encapsula tras un trait `VtEngine` y se compara en Fase 2 (vtebench + conformance con `esctest`/`vttest`).
- **PTY**: `portable-pty` (openpty en Unix, ConPTY en Windows ≥ 1809). PowerShell/cmd vía ConPTY; WSL como shell configurable.
- **Hilos en termd**: un loop tokio para lectura de todos los fds (epoll/kqueue/IOCP); parsing en un pool rayon; cada terminal es `Mutex<Term>` — sin contención real porque un terminal lo parsea un solo worker a la vez.
- **Damage**: tras cada lote de bytes parseados, `termd` calcula filas sucias y envía `Damage { generation, rows: [(row_idx, cells…)] }` coalescido a ≤ 1 mensaje por frame por terminal (el cliente concede créditos por frame). Filas como runs de estilo (`(style_id, text)`) en vez de celda a celda: comprime 5–10× las filas típicas.
- **Input**: la UI envía teclas ya codificadas (xterm o Kitty keyboard protocol según modo del terminal) y ratón (SGR 1006); `termd` escribe al PTY.

### 14.2 Por qué un daemon

1. **Persistencia**: crash o actualización de `forge` no mata `npm run dev`, el `ssh`, ni la sesión del agente TUI. Al rearrancar, la UI hace `attach` y recupera grid y scrollback.
2. **Multiplexing gratis**: varias ventanas/instancias ven la misma sesión; base para `forge attach` desde otra máquina en el futuro.
3. **Aislamiento**: un flood de salida (`cat /dev/urandom`) satura un core en `termd`, no en el hilo de UI.

Coste: IPC por keystroke (objetivo ≤ 0,5 ms, §11.3) y complejidad de reattach. El prototipo lo mide. Feature flag `terminal.in_process = true` usa el mismo código sin socket (transparencia de ubicación).

### 14.3 Integración de shell

Forge inyecta (opcionalmente, `terminal.shell_integration = auto`) scripts para bash/zsh/fish/PowerShell que emiten:

- **OSC 133 A/B/C/D** (prompt start, command start, output start, command end + exit code): navegación por comandos, "copiar salida del último comando", marcas de error en el scrollbar, y la primitiva que los agentes necesitan (`terminal/output` en ACP devuelve salida limpia).
- **OSC 7** (cwd): "abrir archivo relativo al cwd", nuevas pestañas heredan cwd, el editor sabe dónde está la shell.
- **OSC 8** hyperlinks; detección adicional por regex de rutas `file:line:col` → click abre en el editor.
- **OSC 52** (clipboard) solo con permiso (§23).
- **DEC 2026** synchronized output (respetado: no se pinta un frame a medias de una TUI).

### 14.4 Funcionalidad de UI

Splits/tabs: el terminal es un *pane item* como cualquier otro; el árbol de layout es del shell (§4.1). Selección por celda/palabra/línea/bloque con reflow-aware coordinates, búsqueda en scrollback (regex, en `termd`, resultados en stream), scroll con inercia, cursor styles, transparencia, fuente y tema por terminal, "terminal como editor" (abrir scrollback en un buffer). Renombrado automático de tab por título OSC 0/2.

### 14.5 Scrollback y memoria (el problema real de "miles de terminales")

- Filas **calientes** (pantalla + últimas ~2.000 filas): celdas compactas en memoria: `char`/índice de grafema (u32) + `style_id` (u16) → ~6–8 B/celda; estilos en tabla interned por terminal.
- Filas **frías**: bloques de 512 filas serializados como runs y comprimidos con LZ4 (texto de terminal comprime 5–20×).
- Filas **archivadas**: cuando un terminal supera `terminal.scrollback.memory_limit` (32 MB por defecto), los bloques fríos se escriben a `$XDG_RUNTIME_DIR/forge/termd/<id>/` y se cargan bajo demanda al scrollear.
- Presupuesto global `termd` (`terminal.scrollback.global_limit`, 512 MB por defecto): al superarlo, se archivan primero los terminales sin foco.
- Terminal cerrado = PTY cerrado, memoria liberada inmediatamente; terminal "detached" (sin ninguna UI) sigue leyendo el PTY (para no bloquear al proceso) pero solo mantiene la pantalla + scrollback archivado.
- Resultado: 1.000 terminales existentes con 10k líneas → dominado por los archivados en disco; RAM del orden de decenas de MB + pantalla por terminal (~80 KB). 100 activas simultáneamente a pleno rendimiento: CPU, no RAM, es el límite.

### 14.6 Windows

ConPTY tiene diferencias de comportamiento (re-renderiza en lugar de pasar secuencias crudas; latencia extra; sin señales POSIX). Se acepta; `portable-pty` lo abstrae. Las señales se mapean a `GenerateConsoleCtrlEvent`/kill.

### 14.7 SSH y multiplexing

- v1: `ssh` es un comando más en un PTY; con shell integration remota opcional (el usuario instala el script en el host remoto) las OSC 133/7 funcionan a través de SSH.
- Multiplexing tmux-like (attach desde otra máquina, ventanas compartidas): la arquitectura `termd` lo permite; producto **[ABIERTO]**.


---

## 15. LSP

- Cliente propio en `forge-lsp` sobre `lsp-types` (tipos generados de la spec), transporte `Content-Length` sobre stdio (y TCP/socket para servidores que lo exijan). No se usa `tower-lsp` (es para *escribir* servidores).
- **Registro de lenguajes** (`languages.jsonc`, extensible por paquetes y por extensiones VS Code vía `contributes.languages`): id, extensiones, gramática tree-sitter, servidores (comando, args, `initializationOptions`, `rootMarkers`), formateadores, comentarios, indentación. Formato inspirado en `languages.toml` de Helix, en TOML por coherencia (§22).
- **Multi-servidor**: N servidores por lenguaje (p. ej. `typescript-language-server` + `eslint` + `tailwindcss`); las respuestas se fusionan por capacidad (completion: concatenar y deduplicar; hover: apilar; diagnostics: unir por fuente).
- **Ciclo de vida**: arranque en el primer buffer del lenguaje abierto **tras 500 ms** (evitar arrancar al previsualizar), una instancia por (servidor, raíz) con detección de raíz por `rootMarkers`, apagado por inactividad (§10.4), reinicio con backoff, re-`didOpen` tras reinicio.
- **Sincronización**: `didChange` incremental derivado de las transacciones (§13.2), debounce 50–100 ms con flush inmediato antes de cualquier request que dependa del contenido (completion, hover). `didSave`, `willSaveWaitUntil` con timeout 1 s.
- **Prioridad y cancelación**: cola por servidor con prioridades (completion/signature > hover/definition > semanticTokens/inlay > workspace symbols); cancelación (`$/cancelRequest`) al cambiar el cursor o el buffer; coalescing de inlay/semantic tokens por viewport.
- **Diagnostics**: push (`publishDiagnostics`) y pull (`textDocument/diagnostic`, LSP 3.17); gutter + subrayado + panel; sin bloquear el frame aunque lleguen 50k diagnósticos (se recorta la lista renderizada, se indexa por línea).
- **Watched files**: `workspace/didChangeWatchedFiles` alimentado por nuestro watcher (los servidores no deben poner inotify propios sobre `node_modules`; se les declara capability `dynamicRegistration` y se filtran los globs por las exclusiones).
- **Instalación de servidores**: v1 usa lo que hay en `PATH`; Fase 2 de LSP: instalación asistida al estilo Mason/Zed (registro con URLs y checksums) **[ABIERTO]**.
- Presupuesto: overhead de Forge por petición ≤ 1 ms (§5.2); memoria del servidor es del servidor (se muestra en el process explorer).

---

## 16. Git

| Opción | Pros | Contras | Uso |
|---|---|---|---|
| `git` CLI | 100 % compatible: hooks, credential helpers, LFS, sparse, submodules, config global, signing | Spawn ~2–5 ms; parsear salida | **Todas las mutaciones** (commit, checkout, merge, rebase, stash, push/pull, worktree add) y cualquier cosa con red o hooks |
| `gix` (gitoxide) | Rust puro, rápido en repos grandes, sin C, lectura de objetos/índice/refs excelente | Cobertura de escritura incompleta; API en evolución | **Lecturas de alta frecuencia**: status de buffers abiertos, diff buffer↔HEAD para gutters, blame, log de archivo, contenido de objetos para diff views, listar refs |
| `git2` (libgit2) | Completa, conocida | C, más lenta que gix en repos grandes, semánticas que difieren sutilmente de git CLI (config, ignore), problemas históricos con hilos | **Descartada** |

- Status del workspace: `gix` para el índice + watcher; para repos gigantes, `git status --porcelain=v2 -z` con `core.untrackedCache` y `core.fsmonitor` (si está disponible, se recomienda activarlo) como ruta alternativa medida.
- Gutters: diff entre snapshot del buffer y blob de HEAD (o del índice, configurable), cálculo en fondo con `imara-diff` (algoritmo de Myers/histogram, rápido), invalidado por transacción con debounce.
- Diff view / staging por hunk / conflictos 3-way: son vistas del editor (dos/tres buffers alineados), no un componente aparte.
- **Worktrees** de primera clase: crear worktree + abrir como workspace + lanzar agente en él (patrón de aislamiento de agentes).
- Credenciales: siempre vía `git credential` (helpers del sistema); Forge no almacena tokens de git.
- Sin `.git`: el subsistema no se inicializa (lazy).

---

## 17. Agent Host

### 17.1 Por qué ACP y no una abstracción propia

Estado del arte (septiembre 2026): **ACP (Agent Client Protocol)**, iniciado por Zed, es JSON-RPC 2.0 sobre stdio entre un *cliente* (editor) y un *agente* (subproceso). Lo soportan nativa o vía adaptador Claude Code, Codex CLI, OpenCode, Gemini CLI, GitHub Copilot CLI y más de 25 agentes; existen SDKs en Rust, TypeScript, Python, Go y un registro oficial de agentes. Zed y JetBrains lo implementan como clientes; Neovim/Emacs con plugins.

Además, cada agente tiene su protocolo "de casa": Claude Code expone el Claude Agent SDK (stream-json bidireccional por stdio, `canUseTool` para aprobaciones, hooks); Codex expone `app-server` (JSON-RPC JSONL por stdio/unix socket/WebSocket; `codex mcp-server` está deprecado desde v0.149); OpenCode expone `opencode serve` (HTTP + SSE). Escribir un adaptador propio por agente sería duplicar lo que la comunidad ya mantiene (`claude-code-acp` de Zed, adaptador de Codex, etc.).

**Decisión [DECIDIDO]:** Forge es un **cliente ACP**. El trait interno es un adaptador fino, no una abstracción nueva:

```text
Editor → Agente (ACP)                       Agente → Editor (ACP)
─────────────────────                       ─────────────────────
initialize / authenticate                   session/update (notif.: texto, tool calls, planes, diffs)
session/new (cwd, mcpServers[])             session/request_permission (opciones: allow once/always, deny…)
session/load (resume)                       fs/read_text_file  ← ve buffers SIN guardar
session/prompt (bloques: texto, @archivo,   fs/write_text_file ← pasa por "ediciones propuestas"
   selección, imagen)                       terminal/create · terminal/output · terminal/wait_for_exit
session/cancel                                · terminal/kill · terminal/release  ← en forge-termd
session/set_mode (plan/edit/auto)
```

Comparado con la propuesta del brief: `start`≈`initialize`+`session/new`, `send`≈`session/prompt`, `interrupt`≈`session/cancel`, `events`≈`session/update`, `capabilities`≈`initialize`, `shutdown`≈cierre del proceso. Lo que faltaba es toda la columna derecha, y es la que hace que "el agente trabaje sobre el workspace sin que el editor simule una terminal".

### 17.2 Componentes

- `AgentRegistry`: agentes disponibles (config del usuario + ACP registry + detección en `PATH`), con comando de lanzamiento, args, env allowlist, y modo de auth.
- `AgentProcess`: spawn + transporte JSON-RPC + heartbeat por liveness; reinicio con `session/load` cuando el agente lo soporta.
- `Session`: estado de conversación (bloques, tool calls, plan), buffers de contexto, permisos concedidos, ediciones propuestas, terminales creadas; serializable para reabrir tras reinicio de la UI.
- `PermissionBroker`: recibe `session/request_permission`, consulta la política (§23), muestra tarjeta en UI si es `ask`, recuerda decisiones por (agente, herramienta, patrón de ruta/comando).
- `ProposedEdits`: overlay por buffer (§17.5).
- `AgentTerminals`: las llamadas `terminal/*` se cumplen en `forge-termd`; la terminal aparece en la UI como pestaña "Codex: npm test" con la salida real; OSC 133 permite devolver salida limpia y exit code.

### 17.3 Agentes sin ACP

Adaptador ACP como subproceso (en el lenguaje que haga falta: Node para el Claude Agent SDK si `claude-code-acp` no bastara; Rust para HTTP+SSE como OpenCode). Nunca se añaden ramas "si es Codex…" al core. Un agente que solo sea una TUI se ejecuta en una terminal normal: sigue siendo útil, solo que sin integración.

### 17.4 Contexto y prompts

Bloques de prompt ACP: texto, `@archivo` (contenido o referencia por ruta según capacidad del agente), selección actual con rango, diagnósticos del archivo, salida del último comando de terminal (vía OSC 133), imágenes. El usuario ve exactamente qué se envía.

### 17.5 Ediciones propuestas y diff review

1. `fs/write_text_file(path, content)` del agente **no toca disco**: se convierte en una transacción contra la versión actual del buffer (diff Myers entre buffer y `content` → edits) y entra en un **overlay** del buffer marcado como "propuesto por <agente>".
2. La UI muestra los hunks inline (verde/rojo) con aceptar/rechazar por hunk, por archivo o por sesión; modo `auto` los acepta y guarda automáticamente si la política lo permite.
3. Si el usuario edita mientras hay propuestas pendientes, la propuesta se **rebasea** sobre la nueva versión (transformación de rangos); si hay solapamiento, el hunk se marca en conflicto y se rechaza salvo decisión manual.
4. Al aceptar, se aplica como transacción normal (undo funciona), se escribe a disco y se notifica al agente.
5. Archivos no abiertos: el overlay vive en un buffer oculto; se abre en el diff review.

Esto es lo que Zed y Cursor hacen; la diferencia es que aquí es un mecanismo genérico de `forge-buffer`, también usable por extensiones (`WorkspaceEdit` de VS Code con preview).

### 17.6 Paralelismo

Varias sesiones simultáneas; por defecto cada sesión con permisos de escritura recibe un **worktree** propio (opción `agent.isolation = worktree | shared`), lo que evita que dos agentes pisen el mismo árbol. Presupuesto de UI: el streaming de N sesiones se renderiza con virtualización (solo la sesión visible se pinta por token; las demás actualizan contadores).

### 17.7 Proveedores de modelos y enrutamiento por clase de tarea **[RECOMENDADO → Fase 4]**

El agente es el que habla con el modelo; Forge decide **qué agente con qué proveedor** atiende cada petición. Tres capas:

```text
  petición ("instálame ripgrep" · "refactoriza reports/" · Ctrl+K sobre una selección)
      │
      ▼
  Router: clase de tarea ──► (agente, perfil de proveedor, modelo)
      │        trivial  → agente barato + modelo gratuito/local
      │        normal   → gateway con enrutado propio
      │        compleja → Codex con la suscripción de ChatGPT / Claude Code
      │        sin red  → modelo local
      ▼
  Agente ACP (Codex · Claude Code · OpenCode · …) lanzado con el perfil inyectado
      │
      ▼
  Proveedor: ChatGPT/OpenAI · gateway OpenAI-compatible (Token Harbor…) · Anthropic · llama.cpp/ollama
```

- **Perfiles de proveedor** (`agents.jsonc`, §22.3): tipo (`chatgpt`, `openai-compatible`, `anthropic`, `local`), endpoint, credencial (por keyring del SO o `env`), modelo por defecto, coste declarado (`free | subscription | metered`) y límites conocidos. Forge **no** implementa clientes HTTP de modelos: traduce el perfil a lo que cada agente entiende (Codex: `model_providers` + login con ChatGPT; OpenCode: `provider`; Claude Code: `ANTHROPIC_BASE_URL`/`ANTHROPIC_AUTH_TOKEN` o su config de gateway) y lo pasa por env allowlist o archivo de config temporal al lanzar el proceso.
- **Clases de tarea**: `trivial`, `normal`, `complex`, `offline` (extensible). La clase la fija, en este orden, (1) el usuario (`/trivial`, `/codex`, palette, atajo), (2) el comando que originó la petición (`agent.explain` es trivial; `agent.investigate` con diagnóstico + diff es normal; `agent.refactor` sobre varios archivos es compleja), (3) una heurística local barata (tamaño del contexto, número de archivos, presencia de tests). No hay un LLM clasificando peticiones en v1.
- **Transparencia**: cada mensaje del panel muestra ruta, agente y coste declarado; el usuario puede reenviar la misma petición por otra ruta con un clic. Si el agente expone uso o cuota (Codex con plan de ChatGPT), se muestra.
- **Fallback**: proveedor caído o cuota agotada → siguiente ruta compatible de la misma clase, avisando; `offline` es el último escalón.
- **Token Harbor** como ejemplo de gateway: según la conversación que origina esta sección, ofrece una API compatible con OpenAI, rutas gratuitas seleccionadas, un modelo orquestador (`th-orchestra`) para fases de planificación/construcción/revisión y un conector oficial para Codex, Claude Code y OpenCode. **[VERIFICAR]** contra su documentación antes de la Fase 4; el diseño no depende de ello (cualquier endpoint compatible con OpenAI encaja en `openai-compatible`).

### 17.8 Acciones contextuales: la IA en el flujo, no en un panel **[RECOMENDADO → Fase 4]**

Sin "AI panel" omnipresente. El agente se invoca desde donde está el trabajo y llega como **una superficie más** (pane, dock o split; `Ctrl+Shift+A` lo abre o lo enfoca), no como una barra fija.

| Origen | Comando (§6.1 principio 3) | Contexto que se adjunta solo |
|---|---|---|
| Selección en el editor + `Ctrl+K` | `agent.ask` (explicar / cambiar / preguntar) | archivo, rango, texto seleccionado, símbolos del rango vía LSP |
| Diagnóstico LSP (gutter, hover, panel de problemas) | `agent.investigate` | archivo, línea, diagnóstico, definición/referencias del símbolo, `git diff` del archivo |
| Error en la terminal (OSC 133: comando con exit ≠ 0) | `agent.investigate` | comando, salida limpia, cwd, diff del workspace |
| Test fallido (tasks/`tasks.json`) | `agent.investigate` | salida del test, archivos tocados según el diff |
| Hunk en diff review | `agent.ask` | hunk, archivo, propuesta original |
| Palette / `forge --command agent.prompt` | `agent.prompt` | lo que el usuario escriba + @-menciones |

Reglas: el usuario **ve exactamente** el contexto antes de enviarlo (§17.4) y puede quitar piezas; el contexto se construye con los mismos comandos que expone MCP (§18), así que no hay una segunda implementación; el comando fija la clase de tarea por defecto (§17.7). Las respuestas con ediciones entran en el overlay de ediciones propuestas (§17.5); las respuestas con acciones (ejecutar tests) pasan por `PermissionBroker`.

### 17.9 Una sola caja de herramientas para humano y agente **[DECIDIDO]**

Cada capacidad que Forge implementa para el humano es un comando con schema, y ese mismo comando es una tool para el agente vía `forge mcp-server` (§18). No existe una "API de herramientas para IA" separada.

| Herramienta | Humano | Agente (MCP) | Fase |
|---|---|---|---|
| Archivos y buffers | explorador, editor | `forge/read_buffer` (incluye no guardado), `forge/propose_edit` | 4 |
| Terminal | `forge-termd`, pestañas | `terminal/*` de ACP y `forge/run_in_terminal` con permiso | 4 |
| Git | gutters, staging, worktrees | `forge/git_status`, `forge/git_diff`, worktree por sesión | 4 (status/diff), 6 |
| Búsqueda | palette, buscar en workspace | `forge/search` (mismo índice) | 4 |
| LSP | diagnósticos, navegación | `forge/diagnostics`, `forge/workspace_symbols`, definición/referencias | 5 |
| Tareas y paquetes | `tasks.json`, gestor de paquetes detectado | `forge/run_task` con permiso | 5–7 |
| Docker / navegador | **[ABIERTO]**: solo si un uso real lo justifica; antes, vía MCP servers de terceros | 9 |
| MCP de terceros | config §18 | passthrough por ACP | 4 |

Lo que el agente hace con esas herramientas se muestra como **línea de tiempo** en su superficie (leyó 12 archivos, modificó `reports/query.go`, tests en verde, "Revisar cambios"), con cada entrada navegable al archivo, hunk o terminal correspondiente. Es la misma información que la traza `session/update`, no una segunda fuente.

---

## 18. MCP

- **Cliente**: `mcp_servers` en config (usuario/workspace; los del workspace requieren trust), transporte stdio o Streamable HTTP, con `rmcp`. Forge lista tools/resources/prompts y las pasa al agente en `session/new.mcpServers` para que **el agente conecte directamente** (ACP lo prevé). Forge no proxya por defecto: proxyar exige reimplementar toda la semántica MCP y añade un salto; los permisos ya se cubren con `session/request_permission` del agente. Un "MCP gateway" con auditoría es **[ABIERTO]** para entornos que lo exijan.
- **Servidor** (`forge mcp-server`, stdio; y endpoint HTTP local con token para agentes que no acepten stdio): herramientas iniciales `forge/list_open_files`, `forge/read_buffer` (incluye no guardado), `forge/diagnostics`, `forge/workspace_symbols`, `forge/git_status`, `forge/run_in_terminal` (con permiso; devuelve salida limpia vía OSC 133), `forge/propose_edit` (entra en el overlay §17.5); recursos `forge://buffer/<path>`. Permite que **cualquier** agente, incluso uno que corra en una terminal normal sin ACP, use el editor como fuente de verdad.
- Las tools MCP que Forge expone son comandos del registro con schema (§6.1 principio 3): no hay una segunda superficie que mantener.


---

## 19. Extension Host

### 19.1 Dos superficies, una sola verdad

- **API nativa de Forge** (`forge-ext`): traits Rust de *providers* (completion, hover, tree view, terminal, SCM, tasks, debug adapter…), registro de comandos, configuración con schema, vistas. Es lo que usan las features integradas. Es la única fuente de verdad.
- **Shim VS Code** (`ext-host/`, TypeScript en Node): implementa el namespace `vscode` traduciendo a mensajes contra la API nativa. Es *un cliente más* de esa API. Nunca hay un camino especial "para VS Code" en el core.

Consecuencia: la cobertura VS Code crece cuando crece la API nativa, y viceversa; y en el futuro un sistema de extensiones nativas (WASM, §21.3) se apoya en lo mismo.

### 19.2 Proceso `forge-exthost`

- Node.js **bundled** (LTS, versión fijada, ~50 MB en disco, sin `npm`), lanzado con `--max-old-space-size` según presupuesto, `NODE_OPTIONS` limpio, `cwd` en un directorio de trabajo del host (no en el workspace), env allowlist.
- Bootstrap `exthost.js`: (1) conecta al socket; (2) `initialize` con lista de extensiones y sus manifiestos ya parseados por el core; (3) instala el **hook de `require('vscode')`** (interceptación de `Module._load`, igual que VS Code) que devuelve el shim ligado al id de extensión (para atribuir recursos y disposables); (4) espera eventos de activación; (5) `activate(context)` con timeout (aviso a los 5 s, no se mata: hay extensiones lentas legítimas).
- Comunicación: JSON-RPC (mismo codec que `extHost.protocol` conceptualmente: proxies `MainThread*`/`ExtHost*`, pero con nuestro esquema) sobre el socket; los `TextDocument` se sincronizan con `didChange` incremental **solo para los documentos que la extensión ha tocado** (VS Code sincroniza todos: coste evitable); tree views y completions con paginación.
- Grupos: por defecto un host para todas las extensiones de confianza; `extensions.isolate = ["publisher.name"]` mueve extensiones a hosts propios (pesadas, no confiables, o que crashean). Un host que crashea se reinicia (§10.2) con notificación.
- Sin extensiones activas → sin proceso. El primer evento de activación lo arranca (~150 ms; aceptable; se puede precalentar con `extensions.prewarm = true` tras el primer frame).

### 19.3 Aislamiento y límites

- **Bloqueo**: imposible por construcción (proceso aparte, API asíncrona). Todo provider tiene timeout (completion 2 s, hover 3 s, format 10 s, configurable) tras el que se cancela con `CancellationToken` y se registra en el process explorer ("eslint tardó 4,2 s").
- **Memoria**: heap V8 limitado + cgroup/Job Object + monitor RSS (§10.3). Al superar el soft limit: aviso "extensión X usa 600 MB"; hard: reinicio del host con la extensión culpable (por atribución de disposables/timers no se puede saber con exactitud quién asigna en V8: se usa `process.memoryUsage()` global + heurísticas de actividad; **limitación honesta**: la atribución por extensión dentro de un mismo host es aproximada; el remedio es aislar en host propio).
- **CPU**: prioridad baja para hosts sin proveedores en el buffer activo.
- **Filesystem/red**: Node tiene acceso total del usuario. Sandboxing real (Landlock/seccomp en Linux, AppContainer en Windows, ninguno práctico en macOS) es **[ABIERTO]** para extensiones no confiables. VS Code tampoco sandboxea extensiones; se dice explícitamente en la UI de instalación.

---

## 20. VS Code compatibility layer

### 20.1 Cómo funciona una extensión (lo que hay que replicar)

- Paquete `.vsix` = ZIP con `extension.vsixmanifest` + `extension/package.json` + código + assets. `package.json` declara `engines.vscode`, `main` (CJS; ESM en extensiones desde 2025), `browser`, `activationEvents`, `contributes` (commands, keybindings, configuration, languages, grammars, themes, iconThemes, snippets, views, viewsContainers, menus, debuggers, taskDefinitions, jsonValidation, colors, walkthroughs…), `extensionDependencies`, `extensionPack`.
- **Activation events**: `onLanguage:`, `onCommand:` (implícito desde 1.74 para comandos contribuidos), `workspaceContains:`, `onView:`, `onDebug`, `onFileSystem:`, `onUri`, `onWebviewPanel:`, `onStartupFinished`, `*`. Modelo perezoso, alineado con la filosofía de Forge.
- El editor lee `contributes` **sin ejecutar JS**. Muchísimas extensiones (temas, iconos, snippets, gramáticas, keymaps) no necesitan host.
- Las extensiones de lenguaje típicas hacen una cosa: `new LanguageClient(...)` de `vscode-languageclient` que lanza un servidor LSP y registra providers. Hacer que **`vscode-languageclient` funcione** desbloquea cientos de extensiones de golpe: es el objetivo de la Tier 1.

### 20.2 Estrategia por tiers

| Tier | Qué | Cobertura estimada (por nº de extensiones en Open VSX; a confirmar con el escáner) | Fase |
|---|---|---|---|
| **0 — declarativo** | themes, iconThemes, productIconThemes, snippets, grammars (TextMate), languages (configuration: comentarios, brackets), keybindings, configuration (schema → settings UI) | ~25–35 % (los temas son una fracción enorme del registro) | 7 |
| **1 — núcleo** | `commands`, `window` (messages, quickPick, inputBox, statusBar, outputChannel, progress, activeTextEditor, visibleTextEditors, onDid*), `workspace` (folders, config, fs, textDocuments, openTextDocument, applyEdit, onDid*, createFileSystemWatcher), `TextDocument`/`TextEditor`/`TextEditorEdit`/decorations básicas, `languages.register*Provider` (completion, hover, definition, references, rename, formatting, code actions, signature, symbols, folding, semantic tokens, inlay hints), `DiagnosticCollection`, `Uri`, `Range/Position/Selection`, `EventEmitter`, `Disposable`, `CancellationToken`, `env` (clipboard, openExternal, appName, machineId), `extensions` | +30–40 % → **~60–70 % acumulado** | 7 |
| **2 — UI y sistema** | `TreeView` + `contributes.views/viewsContainers/menus`, `Webview` panels/views (§20.5), `Terminal` API (createTerminal, sendText, shellIntegration, onDidWrite*), `tasks`, `scm`, `debug` (DAP: DebugAdapterDescriptorFactory, breakpoints, `startDebugging`), `FileSystemProvider`, `TextDocumentContentProvider`, `CustomEditor` (webview), `authentication` (básico), `tests` API | +15–20 % → **~80–85 %** | 8 |
| **3 — cola larga** | `notebooks`, `comments`, `chat`/`lm` (Language Model API), `proposed` APIs, `TextSearchProvider`, `Timeline`, `workspace.onWillCreateFiles` y otros detalles finos | +5 % con esfuerzo desproporcionado | 9+/nunca |

**Nunca compatibles** (razón):
- **Legales/propietarias**: Pylance, C/C++ (ms-vscode.cpptools), Remote-SSH/WSL/Containers/Tunnels, Live Share, GitHub Copilot/Copilot Chat, Visual Studio IntelliCode, .NET/C# Dev Kit. Sus licencias restringen el uso a productos de Microsoft y/o dependen de APIs propuestas y binarios cerrados. Alternativas abiertas: Pyright/basedpyright, clangd, OmniSharp/csharp-ls, agentes ACP en lugar de Copilot.
- **Técnicas**: extensiones que tocan internals (`vscode.env.appRoot` + require de módulos de VS Code, parches de `Module._load` propios, dependencias de `electron`), extensiones que asumen DOM en el host (webviews aparte), extensiones `browser`-only (host web), y las que requieren `proposed` APIs sin fallback.
- **Marketplace**: los Términos de Uso del Visual Studio Marketplace restringen el acceso a productos de Microsoft. Forge usa **Open VSX** (Eclipse Foundation; 1.0.0 en 2026; >10k extensiones; lo usan Cursor, VSCodium, Windsurf, Kiro, Gitpod). Faltarán algunas extensiones no republicadas en Open VSX; se puede instalar `.vsix` manualmente respetando su licencia.

### 20.3 Medir en lugar de estimar: `tools/vscode-api-scan`

Fase 0 entrega `tools/vscode-api-scan`: descarga el top-N (1.000) de Open VSX por instalaciones, extrae `package.json` y los bundles JavaScript, y cuenta referencias estáticas a `vscode.<ns>.<miembro>` y a las claves de primer nivel de `contributes`, ponderadas por instalaciones. La implementación actual es deliberadamente sin dependencias: usa una expresión regular auditable, no AST con `swc`/`oxc`. Por tanto no infiere aliases, imports, propiedades computadas ni uso dinámico; esos casos se excluyen y la tabla se interpreta como un mínimo observable, no como cobertura total.

La salida es `kind,member,extensions,installs`, acompañada por fecha, versión de Node y checksum del CSV en `bench/results/openvsx-<fecha>/`. La guía reproducible y la semántica de cada columna viven en [`tools/vscode-api-scan/README.md`](../tools/vscode-api-scan/README.md); el criterio de aceptación de Fase 0, en [`docs/PHASE_0.md`](PHASE_0.md). La tabla **ordena el backlog** de la Tier 0/1; una fila alta no declara una API compatible. Cada prioridad pasa después por `vscode.d.ts` y una prueba de extensión real antes de entrar en el dashboard público.

### 20.4 Gramáticas y temas TextMate

- Los temas VS Code colorean **scopes TextMate** (`keyword.control`, `entity.name.function`). Con tree-sitter se genera una tabla de mapeo captura → scope (Zed y Helix hacen esto); cubre la gran mayoría de los casos con temas populares. Para lenguajes sin gramática tree-sitter, se soportan gramáticas TextMate como fallback con un motor propio (`onig` para regex Oniguruma + tokenizer por líneas; o `syntect` si su soporte de `.tmLanguage` resulta suficiente) **[ABIERTO → Fase 7]**. Coste ~2–3 semanas; beneficio: cualquier lenguaje raro tiene highlighting el día 1 de instalar su extensión.

### 20.5 Webviews (el segundo problema más difícil)

Necesitan un motor de navegador. Opciones: `wry` (WebView2 / WKWebView / WebKitGTK; ~0 MB extra en disco, RAM del motor del sistema), CEF offscreen (150 MB, control total, compositable en nuestra textura), Servo embebido (Rust, prometedor, incompleto). **[RECOMENDADO → prototipo Fase 8]** `wry` como **vista hija nativa** posicionada sobre el rect del panel: no se puede componer bajo nuestra UI (popups sobre un webview quedan detrás) y en Wayland/WebKitGTK hay limitaciones de subsurface. Se acepta: un webview ocupa un panel completo; nada de Forge se dibuja encima. El `postMessage` bridge, CSP, `asWebviewUri` y `retainContextWhenHidden` se implementan según la spec. Si la experiencia es inaceptable, CEF offscreen es el plan B con su coste de 150 MB.

### 20.6 Depuración (DAP)

Forge implementa un cliente **DAP** propio (`forge-debug`, Fase 8): breakpoints, stepping, variables, watch, call stack, consola. Las extensiones de debug de VS Code aportan el adaptador (`contributes.debuggers` + `DebugAdapterDescriptorFactory` + `DebugConfigurationProvider`) y funcionan sobre nuestra UI. Es el mismo patrón que LSP.

### 20.7 Alternativa descartada: reutilizar el ext host de VS Code

VS Code es MIT; `src/vs/workbench/api/common/extHost*.ts` es el lado extensión del protocolo interno. Vendorizarlo daría semántica exacta, pero: arrastra `vs/base` y `vs/platform` (~200k LOC), el protocolo `MainThread*` es interno y cambia mensualmente, y obligaría a seguir upstream para siempre: es un fork parcial con otro nombre. Theia (la mayor reimplementación existente) lo reimplementó. Se hace lo mismo, usando `vscode.d.ts` como contrato y la suite `vscode-test` de extensiones reales como test de conformidad. Theia publica su lista de compatibilidad: sirve para calibrar prioridades.

---

## 21. Runtime JS

### 21.1 Para el ext host VS Code

| Runtime | Compatibilidad Node | Nativos (`.node`, N-API) | RSS base | Arranque | Embebible | Veredicto |
|---|---|---|---|---|---|---|
| **Node.js** (bundled) | 100 % | Sí | ~40 MB | ~40–80 ms | Como proceso | **[DECIDIDO]** Las extensiones son Node; todo lo demás es una aproximación. `vscode-languageclient`, `child_process`, `net`, `fs`, `crypto`, `worker_threads`, `.node` addons funcionan. |
| Bun | ~95 %; huecos conocidos | N-API sí | ~25 MB | ~10 ms | Como proceso | Alternativa **[ABIERTO]** intercambiable porque el protocolo es el mismo; ganancia: arranque y RAM; riesgo: incompatibilidades sutiles en extensiones grandes |
| deno_core (+ `ext/node`) | Parcial; `node:` compat mejora pero no es completa | Parcial (N-API vía Deno, no vía deno_core solo) | ~20 MB | rápido | Sí (Rust) | Atractivo por embebible, pero convertiría cada hueco de compat en *nuestro* bug |
| V8 embebido (`rusty_v8`) | 0 % — hay que escribir Node | No | bajo | rápido | Sí | No |
| QuickJS (`rquickjs`) | 0 %; intérprete sin JIT, 10–50× más lento | No | ~1 MB | instantáneo | Sí | No para extensiones (ESLint o `typescript` en QuickJS serían inusables) |

### 21.2 Asegurar que Node no se convierte en Electron

El miedo razonable es "acabamos con 400 MB". Salvaguardas: (a) sin extensiones activas no hay proceso; (b) límites (§19.3); (c) documentos sincronizados bajo demanda; (d) el host no tiene DOM ni renderer: es solo el runtime de las extensiones, igual que en VS Code (donde el peso está en el renderer Electron, no en el ext host).

### 21.3 Para scripting del usuario y extensiones nativas **[ABIERTO]**

No en v1. Candidatos cuando haga falta: **WASM (wasmtime + component model)** al estilo de las extensiones de Zed (sandbox por capacidades, cualquier lenguaje, ~1 ms de arranque), Lua (`mlua`, Neovim-style, sin sandbox real), Rhai (Rust-native, pequeño). Requisito: aislamiento por capacidades y presupuesto de memoria por script. Se decide cuando exista un caso de uso que las macros de comandos (§22.4) no cubran.

---

## 22. Sistema de configuración

### 22.1 Formato **[DECIDIDO en Fase 1: TOML]**

| Formato | Pros | Contras |
|---|---|---|
| JSONC + JSON Schema | Las extensiones VS Code ya contribuyen configuración como JSON Schema (validación, autocompletado y settings UI gratis); el editor lo edita con validación; familiar | Verboso; comas |
| **TOML + JSON Schema** | Legible; estándar Rust/Helix/Alacritty/Ghostty, que es lo que espera el usuario terminal-first; los editores validan TOML contra JSON Schema (taplo, Even Better TOML) | Estructuras anidadas profundas (layouts, keymaps con `when`) son incómodas; las contribuciones JSON de extensiones se convierten |
| KDL | Ideal para layouts (Zellij) | Segundo formato; ecosistema pequeño |
| Lua/JS | Poder ilimitado | Config = código no auditable; segundo ecosistema; seguridad |

Decisión tomada al cerrar la Fase 1 con el código en la mano: **TOML** (`~/.config/forge/config.toml`), con el JSON Schema generado desde los tipos Rust con `schemars` (`forge-gui --print-config-schema`, publicado en `docs/config.schema.json`). El layout persistido (`session.json`) es JSON generado por Forge, no editado a mano. Cuando lleguen las extensiones VS Code, sus contribuciones de configuración se exponen bajo su propio prefijo del mismo schema; la sección "JSONC" de este documento queda como alternativa descartada.

### 22.2 Capas (precedencia creciente)

```text
defaults (compiladas)  →  usuario (~/.config/forge/config.toml)          [Fase 1]
  →  perfil activo (~/.config/forge/profiles/<name>/config.toml)
  →  workspace (<repo>/.forge/config.toml)                                [Fase 1]
  →  carpeta (multi-root: por carpeta)
  →  overrides por lenguaje ("[rust]": {…})
  →  runtime (cambios temporales por comando; no se persisten salvo "guardar")
```

Cada valor resuelto sabe de qué capa viene (la UI lo muestra; "¿por qué está esta opción así?"). Recarga en caliente (en Fase 1 por sondeo cada segundo sin redibujar si nada cambió; watcher cuando haya más archivos que vigilar). Claves con `restart_required` explícito (pocas: renderer, runtime de ext host). Hasta que exista el modelo de confianza (§23), la capa de workspace **no puede** fijar `terminal.shell` ni `terminal.args`: un repositorio no elige qué programa ejecuta Forge en la máquina del usuario.

### 22.3 Qué se configura

`config.toml` (comportamiento, fuente, colores, `[[keybindings]]`; en Fase 1 un solo archivo), `themes/*.toml` (temas del usuario sobre una base integrada), `session.json` (layout, tamaño y tema de la última ventana, escrito por Forge), y más adelante `keymap.toml` (bindings con `when`, chords, modos), `layout.toml`, `languages.toml` (registro §15), `agents.toml` (agentes, proveedores de modelos, enrutamiento y MCP servers), `tasks.toml` (compatible con `tasks.json` de VS Code), `profiles/`. El workspace puede aportar `.forge/config.toml` (y más adelante `keymap`, `tasks`, `agents`) bajo trust.

Forma de `agents.toml` para proveedores y enrutamiento (§17.7):

```toml
[providers.openai]
type = "chatgpt"
agent = "codex"
cost = "subscription"

[providers.tokenharbor]
type = "openai-compatible"
endpoint = "https://…/v1"
credential = "keyring:tokenharbor"
model = "deepseek-v4-flash:free"
cost = "free"

[providers.local]
type = "local"
endpoint = "http://127.0.0.1:8080/v1"
cost = "free"

[routing]
trivial = { agent = "opencode", provider = "tokenharbor" }
normal  = { agent = "opencode", provider = "tokenharbor", model = "th-orchestra" }
complex = { agent = "codex", provider = "openai" }
offline = { agent = "opencode", provider = "local" }
```

### 22.4 Comandos, macros, workflows

- `commands`: registro tipado con schema de args; `forge --command editor.goToLine --args '{"line":42}'` y la palette lo exponen.
- Macros en config: `"macros": { "test.current": ["file.save", {"terminal.run": {"cmd": "cargo test ${file.stem}"}}] }` con variables `${file}`, `${selection}`, `${workspace}`, `${git.branch}`. Enlazables a teclas y a eventos simples (`on.save.[rust]`). Cubre el 90 % de los "workflows" sin scripting.
- Keymap: contextos `when` con el mismo motor que VS Code (`editorTextFocus && mode == 'normal' && !suggestWidgetVisible`), resolución determinista (última capa gana, especificidad no importa: predecible), visor de conflictos.

### 22.5 Perfiles

Un perfil = conjunto de settings + keymap + extensiones activas + tema. Cambio de perfil sin reiniciar (salvo claves `restart_required`). Casos: "Rust", "Web", "Presentación", "Agentes-solo".


---

## 23. Seguridad

### 23.1 Amenazas

| Actor | Riesgo | Mitigación |
|---|---|---|
| Extensión maliciosa/comprometida | Acceso total como el usuario; exfiltración; typosquatting | Open VSX con publishers verificados; instalación explícita con resumen de permisos declarados (`contributes`, activation `*`); proceso aparte y reiniciable; hosts aislados para no confiables; auditoría de spawn/red vía process explorer; Landlock **[ABIERTO]** |
| Agente de IA | Ejecutar comandos destructivos, borrar/editar fuera del workspace, filtrar secrets, prompt injection desde archivos del repo | ACP permissions con política; ediciones **siempre** por overlay revisable salvo modo `auto` explícito; terminales del agente con cwd en el workspace y env allowlist; `fs/*` restringido al workspace (y a rutas aprobadas); comandos peligrosos (patrones `rm -rf`, `git push --force`, `curl … \| sh`) en `ask` aunque el modo sea `auto`; secrets nunca inyectados salvo allowlist explícita |
| Servidor MCP | Herramientas con efectos secundarios; servidores del workspace (repo malicioso) | Los MCP del workspace requieren trust y confirmación por servidor; listado de tools antes de habilitar; HTTP solo con TLS o localhost |
| Workspace no confiable | `.forge/` con tasks, agentes, MCP, LSP command configurados por el repo | **Workspace trust** (como VS Code): sin trust no se leen `.forge/`, no se activan `workspaceContains`, no se arrancan LSP definidos por el repo, los agentes van en modo lectura/plan |
| Terminal | OSC 52 (clipboard), OSC 8 con `file://` → ejecución, paste de texto con saltos de línea, títulos que engañan | OSC 52 write con permiso; hyperlinks solo abren esquemas seguros; **paste protection** (mostrar el pegado multi-línea antes de enviar); bracketed paste |
| Secrets | Tokens de agentes/MCP en config en texto plano | `keyring` (Keychain/Secret Service/Credential Manager); referencias `${secret:NAME}` en config; nunca en `.forge/` del workspace |
| Procesos externos | LSP/agentes heredan env con secrets | Env allowlist por tipo de proceso; `FORGE_*` mínimo; no se propaga `ANTHROPIC_API_KEY`/`OPENAI_API_KEY` salvo al agente que corresponde |
| IPC local | Otro proceso del usuario conecta a `termd`/`exthost` | Sockets 0600 en `$XDG_RUNTIME_DIR`; token de sesión en handshake; named pipes con DACL del usuario |

### 23.2 Modelo de permisos

`Policy { subject: Extension|Agent|McpServer|Task, capability: fs.read|fs.write|process.spawn|net|terminal|clipboard|secrets, scope: glob|command-pattern|host, decision: allow|ask|deny, ttl: once|session|always }`. Decisiones persistidas por workspace (`.forge/permissions.jsonc`, bajo trust) y por usuario. Todo `ask` produce una tarjeta uniforme con las opciones que ACP propone (allow once / always / reject). Registro de auditoría local (quién pidió qué, qué se decidió), consultable.

### 23.3 Supply chain propio

`cargo-deny` (licencias/advisories), `cargo-vet`/`cargo-crev` para crates críticos, lockfiles, builds reproducibles donde sea posible, firma de binarios (notarización macOS, Authenticode), SBOM. Node bundled con verificación de hash.

---

## 24. Modelo de memoria

### 24.1 Presupuestos (R1, workspace grande sin extensiones)

| Componente | Presupuesto | Mecanismo de control |
|---|---|---|
| Binario + runtime + GPU device/driver | 30–50 MB | Fijo; medir por plataforma |
| Atlas de glifos (VRAM + copia CPU opcional) | ≤ 64 MB VRAM, ≤ 8 MB RAM | LRU con tamaño máximo |
| Buffers abiertos | tamaño del texto × ~1,3 (rope) + árbol tree-sitter (~10–20× el texto en peor caso; se descarta en modo grande) | árboles solo para buffers visibles/recientes; descartar tras N min sin ver |
| Índice de rutas | ≤ 50 MB / 1 M rutas | `fst` |
| Índice de símbolos | ≤ 100 MB / `linux` | en disco (SQLite/mmap), caché caliente acotada |
| Cachés de shaping/layout | ≤ 32 MB | LRU |
| Historial undo | ≤ 64 MB por buffer, después se trunca el más antiguo | contador de bytes |
| `termd` | ≤ 512 MB global (config) | scrollback tiers (§14.5) |
| Ext host | ≤ 512 MB soft / 768 hard por host | V8 + cgroup/Job + monitor |
| LSP | sin límite duro; aviso configurable | process explorer |
| **Total core objetivo** | **≤ 250 MB** con `linux` abierto | CI |

### 24.2 Prácticas

- Allocator `mimalloc` (fragmentación baja, buen rendimiento multi-hilo).
- Cero asignaciones en la ruta tecla→frame en estado estacionario: arenas por frame, buffers reutilizados, `SmallVec` para runs.
- Estilos y strings repetidos interned (`lasso`); ids `u32` en vez de `String` en mensajes.
- Todo caché tiene tamaño máximo y métrica exportada; nada crece sin límite "porque suele ser pequeño".
- Ediciones incrementales por defecto en todos los protocolos (LSP, tree-sitter, ext host, journal).
- **Process explorer** (RSS/PSS/CPU por proceso hijo, memoria por caché interno) accesible desde la palette: el usuario ve dónde está la RAM. Es también la herramienta de diagnóstico del equipo.
- Métrica de RSS en CI con umbral por escenario (§28); una PR que sube 10 % la RAM base falla.

---

## 25. Estrategia de lazy loading

Camino crítico de arranque (objetivo ≤ 100 ms): `main` → leer config del usuario (parse TOML, <1 ms medido) → crear ventana + device wgpu (el coste dominante: 20–60 ms según driver) → restaurar layout del último workspace **desde caché serializada** (sin tocar el filesystem del repo) → primer frame con los paneles vacíos o con el último contenido cacheado → **después** de presentar: arrancar `termd`/attach, abrir buffers, walker del proyecto en fondo, watcher, git, gramáticas de los buffers visibles.

| Qué | Cuándo se carga | Cuándo se descarga |
|---|---|---|
| Gramática tree-sitter | primer buffer del lenguaje visible | tras N min sin buffers del lenguaje |
| LSP | primer buffer del lenguaje tras 500 ms | inactividad (§10.4) |
| Ext host | primer evento de activación | sin extensiones activas tras N min |
| Extensión | su `activationEvent` | con el host |
| Fuentes | solo las caras usadas | — |
| Temas | solo el activo | — |
| Git | solo si hay `.git`; blame/log bajo demanda | — |
| Índice de rutas | tras el primer frame, en fondo, incremental por watcher | — |
| Índice de símbolos | en fondo, prioridad baja, tras el índice de rutas; pausable; no corre con batería baja | — |
| Terminal shell integration | al spawn del shell | — |
| Agentes | al crear sesión | al cerrarla |
| Webview engine | primer webview visible | último webview cerrado |
| Scrollback frío | al scrollear | por presión de memoria |

Regla: **nada escanea `node_modules`, `.git`, `target`, `vendor`, builds ni cachés**, nunca, salvo petición explícita del usuario (§13.6).

---

## 26. Crash recovery

| Fallo | Recuperación |
|---|---|
| Panic en un subsistema del core (p. ej. parser de sintaxis) | `catch_unwind` en la frontera del subsistema; se degrada (sin highlighting) y se reporta; el hilo de UI no cae |
| Crash del proceso `forge` | Journal de buffers (append-only por buffer no guardado, fsync ≤ 1 s, formato `(version, edit)`) → al reabrir, "Recuperar N archivos"; `termd` sigue vivo → reattach automático; layout persistido cada 5 s; minidump (`minidumper`) con símbolos para diagnóstico local (envío solo opt-in) |
| Crash de `termd` | Es el único punto donde se pierden terminales: se minimiza su superficie (sin plugins, sin UI), fuzzing del parser VT, y `forge` lo rearranca; los procesos hijos de PTYs huérfanos reciben SIGHUP (comportamiento estándar de terminal cerrada) |
| Crash de ext host | Reinicio con backoff, re-activación, notificación; tras 3 fallos, se desactiva la extensión sospechosa (última activada / la que estaba en un provider) |
| Crash de LSP | Reinicio con backoff, re-`didOpen`; los diagnósticos anteriores se conservan hasta que lleguen nuevos |
| Crash de agente | Notificación; `session/load` si el agente soporta resume; el historial de la sesión vive en Forge y se re-muestra |
| Device GPU perdido | Recrear device/surface/atlas; sin pérdida de estado |
| Config inválida | Nunca impide arrancar: se usa la última válida + aviso con la línea del error |
| Actualización de Forge | `termd` compatible por versión de protocolo; si cambia el protocolo mayor, aviso "las terminales se reiniciarán" |

---

## 27. Testing

| Nivel | Qué | Herramientas |
|---|---|---|
| Unit | rope/transacciones/undo/selecciones, keymap resolver, config layers, framing IPC, políticas de permisos | `cargo test`, `proptest` (invariantes: undo∘redo = id; rebase de ediciones propuestas) |
| Conformidad VT | secuencias ANSI/DEC, modos, reflow | suites de `alacritty_terminal`, `esctest`, `vttest` scripted; comparación grid-a-grid con Alacritty como oráculo |
| Fuzzing | parser VT, TOML/config, framing IPC, protocolo ACP/LSP entrante, gramáticas | `cargo-fuzz` en CI nocturno |
| Render | snapshots de paneles (texto, selección, diagnósticos) renderizados offscreen con lavapipe, comparación con tolerancia | wgpu headless |
| Core headless | escenarios de usuario scriptados contra `forge-core` sin GPUI ("abrir X, teclear Y, esperar diagnóstico Z") | harness propio; base para los benchmarks |
| Protocolos | LSP contra servidor mock + rust-analyzer/tsserver reales; ACP contra un agente mock + Claude Code/Codex/OpenCode/Gemini reales (nightly, con claves de CI); MCP cliente/servidor contra `rmcp` examples | CI por matriz |
| **Extension compat CI** | instala top-N de Open VSX en modo headless, activa, ejecuta un comando canónico, captura `NotImplemented`/errores de API → **dashboard de cobertura** por extensión y por API | `tools/vscode-api-scan` + ext host headless |
| E2E | flujo completo con UI real (Linux CI con Wayland virtual `cage`/`weston headless`; macOS runner; Windows runner) | driver de input sintético |
| Rendimiento | §28 | `forge-bench` |

---

## 28. Benchmarking

### 28.1 Suite `forge-bench` (desde Fase 1)

- **Escenarios end-to-end**, no microbenchmarks: `startup_empty`, `startup_workspace_linux`, `type_1000_chars_rust_file`, `scroll_10k_lines`, `open_1gb_log`, `search_linux_literal`, `fuzzy_1m_paths`, `terminal_cat_1gb`, `terminal_input_latency_under_flood`, `lsp_completion_overhead`, `index_linux_symbols`, `ext_host_activate_top50`, `idle_60s`.
- Cada escenario emite JSON: mediana/p95 de duración, PSS por proceso, frames, CPU. Umbrales versionados en `bench/thresholds.toml`; una regresión > 10 % en p95 bloquea el merge.
- Runner dedicado bare-metal (no VMs cloud ruidosas); los perfiles R1/R2/R3 documentados; `hyperfine` para startup; `perf`/`Instruments` para diagnóstico; trazas `tracing` exportables a Perfetto en cualquier escenario con `FORGE_TRACE=1`.
- Micro (criterion) solo para rope, framing y VT parser: donde una regresión es detectable a nivel de función.

### 28.2 Comparativa externa

Misma máquina, mismos corpus, misma metodología, resultados publicados con fecha y versiones:

| Métrica | Contra | Herramienta |
|---|---|---|
| Startup | VS Code, Zed, Neovim, Helix, Alacritty, Kitty, Ghostty | `hyperfine` con flag/observación de ventana |
| RAM (PSS) idle y con `linux` | idem | `smem`/`/proc/*/smaps_rollup`, `footprint` en macOS |
| CPU idle / durante edición | idem | `perf stat`, `powermetrics` |
| Input latency | VS Code, Zed, Neovim (GUI), Alacritty, Kitty | Typometer + fotodiodo si es viable |
| Terminal throughput | Alacritty, Kitty, WezTerm, Ghostty | `vtebench` |
| Búsqueda | ripgrep (oráculo), VS Code, Zed | `rg --stats` |
| LSP overhead | Zed, VS Code, Neovim | timestamps en cliente + servidor instrumentado |
| Apertura de archivos grandes | idem | escenario |

Principio: **ninguna optimización sin un escenario que la justifique y una medida antes/después en la PR**.


---

## 29. Roadmap

Reordenado respecto al brief: **terminal antes que editor**, **agentes antes que LSP**, y sin fase de "optimización" (es transversal). Duraciones para 2–4 ingenieros con agentes; son estimaciones para planificar, no compromisos.

| Fase | Nombre | Duración | Entregable usable |
|---|---|---|---|
| 0 | Decisiones y spikes | 3–4 sem | Este documento cerrado; 3 prototipos con números; escáner VS Code API |
| 1 | Shell de aplicación | 3–4 sem | Ventana con layout, palette, keymap, config, temas; benchmarks en CI |
| 2 | Terminal | 5–6 sem | **Emulador de terminal usable a diario** |
| 3 | Editor core | 6–8 sem | Editor de texto rápido con tree-sitter, búsqueda, archivos grandes |
| 4 | Agentes (ACP) + MCP | 4–6 sem | **MVP terminal-first**: terminal + editor + agentes con diff review |
| 5 | LSP | 5–7 sem | IDE para lenguajes con servidor LSP |
| 6 | Git | 3–4 sem | Gutters, diff, staging, worktrees |
| 7 | Extension host + Tier 0/1 | 8–12 sem | Temas, gramáticas, extensiones de lenguaje reales |
| 8 | Tier 2 + debug + Windows paridad | 8–12 sem | Tree views, webviews, terminal API, DAP |
| 9 | Ecosistema y cola larga | continuo | Dashboard de cobertura; mejoras dirigidas por datos |

Transversal desde Fase 1: presupuestos de rendimiento en CI, crash recovery, process explorer, docs, seguridad.

### Fase 0 — Investigación, decisiones y spikes (3–4 semanas)

- **Objetivo**: convertir las etiquetas **[RECOMENDADO → prototipo]** en **[DECIDIDO]** o en su alternativa, con números.
- **Tareas**: (1) spike renderer: GPUI vs winit+wgpu (§ Primer prototipo); (2) spike `termd`: PTY + VT + IPC + render, medir latencia; (3) spike ACP: cliente mínimo hablando con `claude-code-acp`, Codex y Gemini CLI: `session/new` → `prompt` → recibir `fs/read_text_file` y `request_permission`; (4) `tools/vscode-api-scan` sobre top-1.000 Open VSX; (5) `bench/MACHINES.md` + medir baselines de VS Code/Zed/Neovim/Helix/Alacritty/Kitty/Ghostty; (6) esqueleto del workspace Cargo + CI (Linux/macOS/Windows build, lavapipe) + `cargo-deny`.
- **Dependencias**: ninguna.
- **Riesgos**: GPUI no compila en Windows o su custom element no sirve para grids (→ fallback); latencia IPC de termd > 1 ms (→ in-process con feature flag).
- **Benchmark**: los del prototipo (§ Primer prototipo).
- **Aceptación**: tabla de decisiones §8.2 sin "Media/Baja" en renderer, terminal e IPC; tabla de cobertura de API real; baselines publicados.
- **No implementar**: nada de producto.

### Fase 1 — Shell de aplicación (3–4 semanas)

- **Objetivo**: la "cáscara" completa sin contenido: ventana, event loop, layout de panes/docks/tabs, comandos, keymap, palette, config con capas y schema, temas y fuentes, process explorer vacío, `forge-bench` con `startup_empty`, `idle_60s`.
- **Arquitectura**: `forge-app`, `forge-ui`, `forge-core`, `forge-ipc`, `forge-supervisor`, `forge-bench`.
- **Tareas**: árbol de layout con persistencia; comandos + keymap `when`; TOML + schema + recarga; tema; IME/DPI/multi-monitor; diálogos nativos; tracing + Perfetto; CI con umbrales. Registro de ejecución y cierre: `docs/PHASE_1_EXECUTION.md`.
- **Dependencias**: Fase 0.
- **Riesgos**: IME y Wayland fraccional en GPUI; tiempo de compilación.
- **Benchmark**: startup ≤ 100 ms; RSS ≤ 60 MB (sin termd); 0 frames idle; frame ≤ 2 ms con 20 paneles vacíos.
- **Aceptación**: se puede abrir, dividir paneles, cambiar tema y keymap, reiniciar y recuperar el layout; benchmarks verdes en CI en las 3 plataformas (Windows puede ser "build only").
- **No implementar**: editor, terminal, cualquier contenido de panel.

### Fase 2 — Terminal (5–6 semanas)

- **Objetivo**: reemplazar el emulador diario del equipo.
- **Arquitectura**: `forge-termd`, `forge-term`, `forge-text` (grid renderer).
- **Tareas**: daemon + attach/detach + reconexión; PTY (Unix + ConPTY); `alacritty_terminal` tras trait `VtEngine`; damage por runs con créditos; grid renderer con atlas, celdas anchas, emoji, cursor styles, subrayados; input (xterm + Kitty keyboard); mouse; selección + copy/paste + bracketed paste + paste protection; scrollback tiers + búsqueda; OSC 133/7/8/52 + scripts de integración para 4 shells; hyperlinks y detección `file:line`; tabs/splits/rename; settings por terminal; señales y cierre limpio.
- **Dependencias**: Fase 1.
- **Riesgos**: ConPTY; reattach con reflow; rendimiento del render de runs; corner cases de shell integration en configuraciones del usuario (oh-my-zsh, starship, tmux).
- **Benchmark**: vtebench ≥ 0,8× Alacritty; latencia tecla→eco ≤ 8 ms p99 (R1); input latency ≤ 16 ms bajo `cat` de 1 GB; RSS/terminal idle ≤ 2 MB; 200 terminales abiertas: RSS termd ≤ 100 MB.
- **Aceptación**: `vim`, `htop`, `claude`, `codex`, `tmux`, `fzf` funcionan sin glitches; `vttest` en las secciones soportadas; matar `forge -9` y reabrir recupera todas las terminales con scrollback; el equipo lo usa a diario 2 semanas.
- **No implementar**: gráficos Kitty/Sixel, SSH como workspace, attach remoto.

### Fase 3 — Editor core (6–8 semanas)

- **Objetivo**: editor de texto rápido y correcto, sin LSP.
- **Arquitectura**: `forge-buffer`, `forge-syntax`, `forge-project`, `forge-search`.
- **Tareas**: rope + transacciones + undo + journal; selecciones/multi-cursor; vistas con wrap, folding, gutters, minimap opcional; tree-sitter (highlight, inyecciones, indent, folds, textobjects) con carga lazy de gramáticas; modo archivo grande (mmap); explorador de archivos con `ignore`; fuzzy finder (nucleo + fst); búsqueda/reemplazo en buffer y proyecto (ripgrep crates) con stream; watcher; encodings/EOL; autosave; keymap por defecto + modal Helix-like básico; abrir `file:line:col` desde la terminal.
- **Dependencias**: Fase 1 (Fase 2 para abrir desde terminal).
- **Riesgos**: rendimiento de tree-sitter en inyecciones profundas (Markdown con muchos bloques); reflow/wrap con multi-cursor; watcher en `linux` (límites inotify → fallback).
- **Benchmark**: tecleo p99 ≤ 3 ms interno en archivo de 10k líneas con highlighting; scroll 60/120 fps; `open_1gb_log` ≤ 500 ms; `search_linux_literal` ≤ 1,2× rg; `fuzzy_1m_paths` ≤ 10 ms; RSS con `linux` ≤ 250 MB.
- **Aceptación**: editar Forge con Forge (dogfooding); kill -9 recupera buffers; suite proptest verde.
- **No implementar**: LSP, git, snippets, emulación Vim completa, símbolos de workspace (puede empezar).

### Fase 4 — Agentes (ACP) + MCP (4–6 semanas)

- **Objetivo**: el MVP terminal-first: agentes trabajando en el workspace con revisión.
- **Arquitectura**: `forge-agent`, `forge-mcp`, overlay de ediciones propuestas en `forge-buffer`, `PermissionBroker`.
- **Tareas**: cliente ACP completo (initialize/auth/new/load/prompt/cancel/set_mode/update); panel de sesión con streaming virtualizado, tool calls, planes; `fs/*` sobre buffers; `terminal/*` sobre termd con pestañas visibles; permisos con política y tarjetas; overlay + diff review + rebase; contexto (@archivo, selección, salida de terminal); registro de agentes + ACP registry; MCP cliente (config + passthrough) y `forge mcp-server` con las tools iniciales; worktree por sesión; **perfiles de proveedor** inyectados a Codex/OpenCode/Claude Code y **router** por clase de tarea con ruta visible y reenvío (§17.7); **acciones contextuales** `agent.ask` (`Ctrl+K`) y `agent.investigate` desde diagnóstico y desde comando fallido en terminal (§17.8); superficie del agente como pane (`Ctrl+Shift+A`) con línea de tiempo de herramientas (§17.9); pruebas nightly con Claude Code, Codex, OpenCode, Gemini CLI.
- **Dependencias**: Fases 2 y 3.
- **Riesgos**: diferencias de implementación ACP entre agentes (capacidades opcionales); rebase de propuestas sobre ediciones concurrentes; agentes que escriben a disco por su cuenta ignorando `fs/write_text_file` (se detecta por watcher y se muestra como diff externo).
- **Benchmark**: overhead de Forge por `session/update` ≤ 0,2 ms; render de 10k tokens/min sin frames perdidos; 4 sesiones paralelas sin degradar tecleo.
- **Aceptación**: flujo completo "pedir cambio → ver diff → aceptar por hunk → tests en terminal creada por el agente" con los 4 agentes; permisos recordados; sesión recuperada tras reinicio de Forge; Codex con login de ChatGPT sin configuración adicional; una petición `trivial` atendida por un proveedor `free` sin tocar la cuota de la suscripción; `agent.investigate` sobre un diagnóstico llega al agente con archivo, línea, diagnóstico y diff sin intervención del usuario.
- **No implementar**: MCP gateway/proxy, UI de "chat" genérica con modelos directos (Forge no llama a APIs de LLM por sí mismo: eso lo hacen los agentes; las tareas triviales van a un agente ACP con un perfil barato, no a un cliente propio), clasificador de tareas con LLM.

### Fase 5 — LSP (5–7 semanas)

- **Objetivo**: IDE para cualquier lenguaje con servidor.
- **Tareas**: cliente + registro de lenguajes; multi-servidor; sync incremental; completion (con snippets LSP, resolve lazy, ranking), hover, signature, goto/refs/impl, rename con preview, diagnostics push/pull + panel, code actions + quick fixes, format (doc/rango/on-save), symbols (doc/workspace), folding, semantic tokens, inlay hints, call hierarchy; watched files; prioridades/cancelación; apagado por inactividad; process explorer con LSPs.
- **Dependencias**: Fase 3.
- **Riesgos**: servidores que violan la spec (hay muchos); tsserver vía `typescript-language-server` con proyectos enormes.
- **Benchmark**: `lsp_completion_overhead` ≤ 1 ms; 50k diagnósticos sin frames perdidos; rust-analyzer sobre Forge sin tirones al escribir.
- **Aceptación**: Rust, TypeScript, Python (basedpyright), Go, C/C++ (clangd) con las 12 capacidades funcionando; suite mock verde.
- **No implementar**: instalación automática de servidores (Fase 9), DAP.

### Fase 6 — Git (3–4 semanas)

- **Tareas**: gix para status/blob/blame/log; gutters con `imara-diff`; diff view; staging por hunk; commit con mensaje en buffer; branches/stash/worktrees; conflictos 3-way; integración con watcher y `core.fsmonitor`; credenciales por helper.
- **Benchmark**: status de `linux` ≤ 300 ms (frío) y ≤ 30 ms (caliente con fsmonitor); gutter tras tecleo ≤ 50 ms.
- **Aceptación**: flujo diario de git sin salir de Forge; worktrees para agentes desde la UI.
- **No implementar**: cliente de GitHub/GitLab (PRs), rebase interactivo gráfico.

### Fase 7 — Extension host + VS Code Tier 0/1 (8–12 semanas)

- **Tareas**: instalación desde Open VSX/VSIX con verificación; parser de `contributes`; temas (mapeo tree-sitter→scopes), icon themes, snippets, gramáticas TextMate fallback (decisión §20.4); Node bundled + `exthost.js` + hook `require`; shim `vscode` Tier 1 guiado por el escáner; `vscode-languageclient` funcionando end-to-end; providers sobre la API nativa; timeouts/límites/reinicio; extension compat CI con top-50 de lenguaje; settings UI a partir de `contributes.configuration`.
- **Dependencias**: Fases 5 y 6 (SCM/LSP providers nativos ya existen).
- **Riesgos**: el "casi funciona" de las extensiones (pequeñas diferencias de semántica: orden de eventos, `Uri` normalización, `workspace.fs` en Windows); crecimiento de RSS; extensiones que activan con `*`.
- **Benchmark**: `ext_host_activate_top50` ≤ 3 s total; RSS host ≤ 150 MB con 20 extensiones; ninguna extensión bloquea la UI (por construcción; se prueba con una extensión que hace `while(true)`).
- **Aceptación**: top-50 extensiones de lenguaje de Open VSX activan y funcionan en su flujo principal; 10 temas populares se ven igual que en VS Code (comparación visual); dashboard público.
- **No implementar**: webviews, tree views, debug, terminal API.

### Fase 8 — Tier 2, debug, paridad Windows (8–12 semanas)

- **Tareas**: tree views + menús + viewsContainers; webviews con `wry` (spike primero); Terminal API (incluida shell integration API); tasks (`tasks.json` compat); SCM API; FileSystemProvider; `forge-debug` (cliente DAP + UI) y API de debug de extensiones; testing API básica; accesibilidad inicial (AccessKit) si GPUI lo permite; Windows: ConPTY pulido, named pipes, instalador, firma.
- **Benchmark**: webview arranca ≤ 300 ms; tree view de 100k nodos virtualizado; sesión de debug con 1k breakpoints sin degradación.
- **Aceptación**: GitLens (o equivalente abierto), ESLint, Prettier, Python debug (debugpy), Go debug (dlv), un tema de iconos, y un webview-heavy (p. ej. un visor de Markdown/Mermaid) funcionan; Windows al mismo nivel que Linux/macOS.
- **No implementar**: notebooks, `chat`/`lm` API, proposed APIs.

### Fase 9 — Ecosistema y cola larga (continuo)

Dirigido por el dashboard: cada semana, las 5 APIs faltantes con más instalaciones acumuladas. Instalación asistida de LSPs. Decisiones **[ABIERTO]** que hayan madurado (remote, WASM extensions, multiplexing).

---

## 30. Riesgos técnicos

| Riesgo | Prob. | Impacto | Señal temprana | Mitigación |
|---|---|---|---|---|
| GPUI cambia de forma incompatible o no sirve para nuestro grid | Media | Alto | Spike Fase 0; > 2 días/mes en upgrades | `forge-core` sin GPUI; `forge-text` contra wgpu; `gpui-ce`; fallback winit+wgpu |
| Los agentes no aceptan proveedores inyectados (cambian su config, bloquean gateways) o las condiciones de las suscripciones prohíben el uso vía terceros | Media | Medio | Al integrar cada agente en Fase 4; revisar ToS de ChatGPT/Codex y del gateway | El router degrada a "un agente = su proveedor nativo"; el diseño no depende de ningún gateway concreto |
| IPC de termd añade latencia perceptible | Baja | Medio | > 1 ms en prototipo | Feature flag in-process; runs en vez de celdas; shm más adelante |
| Compatibilidad VS Code se estanca en "casi funciona" | Alta | Alto | Extensiones top-50 con errores sutiles | Escáner + compat CI + priorizar por instalaciones; aceptar que es una cola infinita y comunicarlo |
| Webviews en wry son una experiencia pobre | Media | Medio | Spike Fase 8 | CEF offscreen como plan B (150 MB) |
| Agentes divergen en su implementación de ACP | Media | Medio | Nightly tests | Capacidades opcionales manejadas explícitamente; adaptadores propios solo si es inevitable |
| RSS crece con el tiempo ("death by a thousand caches") | Alta | Medio | CI de RSS | Presupuestos + umbrales + process explorer + LRU en todo |
| Windows llega tarde | Media | Medio | Build CI roja | Windows en CI desde Fase 1 (build), Fase 3 (tests); ConPTY en Fase 2 |
| Tiempos de compilación de Rust frenan la iteración | Alta | Medio | > 30 s incremental | Crates pequeños, `cargo check`, `mold`, cache, `dylib` en dev |
| Licencias: dependencia GPL accidental | Baja | Alto | `cargo-deny` | Política de licencias en CI |
| Reutilizar `alacritty_terminal` limita features (imágenes, reflow) | Media | Bajo | Fase 2 | Trait `VtEngine`; `libghostty-vt` como alternativa |
| Equipo pequeño frente a alcance grande | Alta | Alto | Fases que se alargan > 50 % | Cortar por tiers, no por calidad; el MVP es Fase 4 |

## 31. Decisiones que debemos validar mediante prototipos

Consolidadas en la sección siguiente para no duplicar.

---

# Decisiones que NO debemos tomar todavía

Cada una con la pregunta que el prototipo/medición debe responder y cuándo.

| # | Decisión | Pregunta a responder | Cuándo |
|---|---|---|---|
| 1 | GPUI vs winit+wgpu+propio | ¿Compila y rinde en las 3 plataformas? ¿Custom element para grids sin copiar memoria por frame? ¿Startup/RSS dentro de §5.2? ¿Coste de seguir upstream? | Fase 0 |
| 2 | `termd` como proceso separado | ¿Latencia IPC tecla→eco ≤ 0,5 ms de overhead? ¿Reattach fiable? | Fase 0 (spike), Fase 2 |
| 3 | Shared memory para grids/archivos | ¿Los mensajes por runs saturan CPU o socket con 20 terminales activas? | Fase 2 con bench |
| 4 | `ropey` vs `crop` vs SumTree propio | ¿Necesitamos métricas por nodo (alturas de wrap)? ¿Diferencia medible en `type_1000_chars`? | Fase 3 |
| 5 | `alacritty_terminal` vs `libghostty-vt` | ¿Estabilidad de la API de libghostty? ¿vtebench y conformance? ¿Reflow, imágenes? | Fase 2 |
| 6 | Rasterización de texto: `swash` vs plataforma | ¿Calidad percibida y consistencia con el sistema vs. coste de mantener 3 backends? | Fase 1 |
| 7 | Formato de config JSONC vs TOML | **Decidido: TOML** (§22.1). Queda por validar en Fase 3–4 que keymaps con `when` y layouts sigan siendo legibles en TOML | Fase 1 → revisar en Fase 3 |
| 8 | Separar core y renderer en procesos ("headless core") | ¿Aparece el caso de uso (remote, TUI cliente, multi-ventana entre máquinas)? | Fase 9+ |
| 9 | Node vs Bun para el ext host | ¿Bun pasa la compat CI top-50 sin regresiones? ¿Ganancia real de RSS/arranque? | Fase 7 (tras tener compat CI) |
| 10 | Gramáticas TextMate como fallback | ¿Cuántos lenguajes del top-1.000 no tienen gramática tree-sitter? | Fase 7 con datos del escáner |
| 11 | Motor de webviews: wry vs CEF vs Servo | ¿Es aceptable un webview como vista hija opaca? ¿Wayland? | Fase 8 |
| 12 | Sandboxing de extensiones (Landlock/AppContainer) | ¿Qué rompe? ¿Lo pedirán los usuarios? | Fase 9 |
| 13 | Scripting de usuario / extensiones nativas WASM | ¿Qué workflows no cubren las macros de comandos? | Cuando exista evidencia |
| 14 | MCP gateway con auditoría | ¿Algún entorno lo exige (empresa)? | Fase 9 |
| 15 | Índice de símbolos: SQLite vs formato propio | ¿Tamaño y velocidad de actualización en `linux`? | Fase 3/5 |
| 16 | Suspensión de procesos (SIGSTOP) | ¿Ahorra algo medible frente a apagar+rearrancar? | Fase 9 |
| 17 | Remote development / SSH workspaces | ¿Modelo VS Code (server remoto) o Zed (core remoto)? Depende de #8 | Fase 9+ |
| 18 | Undo tree expuesto en UI | ¿Lo piden los usuarios modales? | Fase 3 (implementar como árbol, exponer después) |
| 19 | Licencia del proyecto | Apache/MIT vs AGPL: afecta a contribuciones y a reutilización de crates | Antes del primer release público |
| 20 | Accesibilidad completa (AccessKit) | ¿GPUI expone árbol accesible completo? | Fase 8 |

---

# Primer prototipo

Lo que un agente de programación (Codex, Claude Code…) debe construir en las **primeras 1–2 semanas** para validar que la arquitectura funciona. No es producto; es un experimento con números. Todo se tira o se conserva según los resultados.

## Objetivo

Responder con medidas a las tres apuestas más caras: **(A)** GPUI sirve para un grid de texto de alto rendimiento y arranca/consume dentro de presupuesto; **(B)** una terminal en proceso separado no añade latencia perceptible; **(C)** un cliente ACP mínimo funciona con Claude Code, Codex y Gemini CLI, incluyendo `fs/read_text_file` y `request_permission`. Más **(D)**: el escáner de API VS Code produce una tabla real.

## Estructura

```text
forge/
  Cargo.toml                 workspace, edition 2024, resolver 3
  rust-toolchain.toml        stable fijado
  crates/
    proto-ui/                bin: ventana GPUI, layout con 2 paneles (terminal | texto), palette mínima
    proto-text/              lib: atlas de glifos + grid renderer (wgpu directo o custom element de GPUI)
    proto-termd/             bin: daemon PTY + alacritty_terminal + socket + protocolo de runs
    proto-term-client/       lib: attach, input, damage → grid cache
    proto-buffer/            lib: ropey + tree-sitter (rust, typescript) + highlight por rango visible
    proto-acp/               bin: cliente ACP por stdio (initialize, session/new, prompt, updates, fs/*, permission)
    proto-ipc/               lib: framing + rmp-serde + tokio Unix/named pipe
    proto-bench/             bin: escenarios y salida JSON
  tools/vscode-api-scan/     TypeScript (Node): descarga top-N Open VSX, analiza con oxc/swc, tabla CSV
  bench/MACHINES.md          hardware exacto de R1/R2/R3
  bench/results/             JSON por fecha
```

Dependencias iniciales: `gpui` (versión fijada; feature `wayland`,`x11`), `wgpu` (la que GPUI use), `cosmic-text` (si se usa fuera de GPUI), `alacritty_terminal`, `portable-pty`, `ropey`, `tree-sitter`, `tree-sitter-rust`, `tree-sitter-typescript`, `tokio`, `rmp-serde`, `serde`, `serde_json`, `tracing`, `tracing-subscriber`, `mimalloc`, `agent-client-protocol` (SDK Rust de ACP), `hyperfine` (externo).

## Plan por días

| Días | Trabajo | Resultado medible |
|---|---|---|
| 1 | Workspace, CI (Linux + macOS; Windows build-only), `proto-ipc` con framing y tests | `cargo test` verde en 3 SO |
| 2–3 | `proto-ui`: ventana GPUI, 2 paneles, texto estático, `--exit-after-first-frame`, tracing con spans `startup` | **startup** con `hyperfine` (N=20); **RSS** (PSS) tras 5 s idle; **frames en 60 s idle** (debe ser 0 ± cursor) |
| 3–5 | `proto-text`: atlas + grid de 200×60 celdas con cambios aleatorios cada frame; medir frame time GPU/CPU | **frame time p95** con grid completo sucio; VRAM del atlas |
| 4–7 | `proto-termd` + cliente: spawn shell, parse con `alacritty_terminal`, runs por fila, créditos, attach/detach; `vtebench`; kill de la UI y reattach | **latencia tecla→eco** (timestamp al enviar Input, timestamp al recibir el Damage con el eco, en el mismo proceso: ~ IPC + parse) p50/p99; **vtebench** vs Alacritty; **RSS termd** con 50 terminales idle |
| 6–8 | `proto-buffer`: abrir un `.rs` de 10k líneas y un `.log` de 1 GB (mmap, sin tree-sitter), tecleo sintético 1.000 chars, highlight por rango | **latencia por tecla** interna p99; **tiempo de apertura** 1 GB; RSS |
| 8–10 | `proto-acp`: `initialize` → `session/new` → `prompt("lee src/main.rs y propón un cambio")` → registrar `session/update`, atender `fs/read_text_file` desde un buffer en memoria (contenido no guardado), responder `request_permission` desde stdin; probar con `claude-code-acp`, `codex` (ACP), `gemini --acp`/adaptador | Tabla: agente × (auth ok, new ok, prompt ok, fs/read recibido, permission recibida, terminal/create recibida, cancel ok) |
| 8–10 (paralelo) | `tools/vscode-api-scan` sobre top-1.000 de Open VSX | CSV `api,member,extensions,installs` + top-50 APIs; % de extensiones sin `main` (Tier 0) |
| 10 | Prueba de fallback: si (A) falla en algún criterio, un día para `winit + wgpu + cosmic-text` con el mismo grid y comparar | Números comparables |
| 11–12 | Informe: `bench/results/<fecha>.md` con tablas, y PR que actualiza §8.2 y §12 de este documento con **[DECIDIDO]** | Documento actualizado |

## Criterios de éxito del prototipo

| Apuesta | Pasa si | Falla si (→ acción) |
|---|---|---|
| A — GPUI | startup ≤ 150 ms (margen sobre 100 ms porque es prototipo), PSS ≤ 100 MB, 0 frames idle, frame p95 ≤ 4 ms con grid sucio completo, compila en Windows, IME básico funciona en Linux/macOS | Cualquiera de los anteriores falla y no se corrige en 1 día → fallback winit+wgpu y repetir |
| B — termd | overhead IPC tecla→eco ≤ 0,5 ms p50 / ≤ 1 ms p99; vtebench ≥ 0,7× Alacritty (en prototipo); reattach recupera pantalla + 10k líneas | > 1 ms p99 → termd in-process con el mismo código (feature flag) y reevaluar en Fase 2 |
| C — ACP | 3 de 3 agentes completan el flujo con `fs/read` desde buffer no guardado y `request_permission` | Un agente falla por bug del adaptador → issue upstream + adaptador propio mínimo solo si es bloqueante |
| D — Escáner | Tabla reproducible; Tier 0/1 estimados con datos | — |

## Qué NO hacer en el prototipo

Sin tabs ni splits reales, sin config, sin temas, sin LSP, sin git, sin undo, sin multi-cursor, sin diff review, sin ext host, sin webviews, sin Windows más allá de compilar, sin optimizar nada que no mida un criterio de arriba, sin abstracciones "para el futuro": el código del prototipo se puede tirar.

---

## Apéndice A — Fuentes consultadas (septiembre 2026)

- ACP: [agentclientprotocol.com](https://agentclientprotocol.com), [Zed — ACP](https://zed.dev/acp), [ACP Registry](https://zed.dev/blog/acp-registry), [ACP en JetBrains y Zed](https://www.danilchenko.dev/posts/agent-client-protocol/), [ACP explicado](https://www.morphllm.com/agent-client-protocol), [Codex CLI en Zed vía ACP](https://codex.danielvaughan.com/2026/05/05/codex-cli-in-zed-parallel-agents-acp-integration-ide-workflows/), [ACP TypeScript SDK](https://agentclientprotocol.github.io/typescript-sdk/classes/AgentSideConnection.html)
- Codex app-server: [README oficial](https://github.com/openai/codex/blob/main/codex-rs/app-server/README.md), [OpenAI — Unlocking the Codex harness](https://openai.com/index/unlocking-the-codex-harness/), [Deprecación de `codex mcp-server`](https://codex.danielvaughan.com/2026/08/25/codex-mcp-server-deprecated-app-server-migration-claude-code-plugin-v0149/)
- Claude Agent SDK: [Permisos](https://platform.claude.com/docs/en/agent-sdk/permissions), [Headless mode](https://www.buildthisnow.com/blog/guide/development/claude-code-headless-mode)
- GPUI: [crates.io/gpui](https://crates.io/crates/gpui), [README](https://github.com/zed-industries/zed/blob/main/crates/gpui/README.md), [gpui-ce](https://github.com/gpui-ce/gpui-ce), [Zed 1.15 stable](https://zed.dev/releases/stable/latest)
- Linebender: [Vello](https://lib.rs/crates/vello), [Xilem](https://docs.rs/xilem/latest/xilem/)
- libghostty: [Libghostty Is Coming](https://mitchellh.com/writing/libghostty-is-coming), [C API overview](https://ghostty-org-ghostty.mintlify.app/api/overview), [ghostling](https://github.com/ghostty-org/ghostling)
- Open VSX: [1.0.0](https://visualstudiomagazine.com/articles/2026/06/24/open-vsx-1-0-0-puts-focus-on-open-extension-registry-for-vs-code-ecosystem.aspx), [Managed Registry](https://newsroom.eclipse.org/news/announcements/eclipse-foundation-launches-open-vsx-managed-registry-0), [FAQ](https://www.eclipse.org/legal/open-vsx-registry-faq/)
