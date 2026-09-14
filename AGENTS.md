# AGENTS.md — Forge

Estas instrucciones se aplican a todo el repositorio.

## Antes de trabajar

- Lee `docs/ARCHITECTURE.md` y el documento de ejecución de la fase activa.
- Para la Fase 4, lee también `docs/PHASE_4_EXECUTION.md` y
  `docs/PHASE_4_TESTING.md`.
- Revisa `git status --short` antes de editar. Los cambios existentes y los
  archivos sin seguimiento pertenecen al usuario: no los borres, sobrescribas
  ni incluyas en commits sin autorización. En particular, no añadas
  `notas.txt`.

## Navegación del código

- Si existe `.codegraph/`, usa primero `codegraph explore` para entender o
  localizar código antes de recurrir a búsquedas textuales.
- Si no existe `.codegraph/` y Graphify está instalado, úsalo primero para
  consultas de arquitectura y relaciones entre componentes.
- Para búsquedas textuales usa `rg` y `rg --files`; evita `grep`/`find` salvo
  que `rg` no esté disponible.

## Arquitectura y alcance

- Respeta las decisiones marcadas como `[DECIDIDO]` en
  `docs/ARCHITECTURE.md`. No sustituyas protocolos o componentes principales
  sin una decisión documentada.
- Forge es terminal-first. Las terminales humanas y las creadas por agentes
  usan `forge-termd`; no implementes una segunda terminal exclusiva para IA.
- ACP es la frontera entre Forge y los agentes. No añadas ramas por nombre de
  agente al núcleo cuando la capacidad pueda expresarse mediante ACP.
- El estado vivo del editor es la fuente de verdad: las lecturas del agente
  priorizan buffers abiertos y las ediciones propuestas se aplican mediante
  `forge-buffer::Transaction`, nunca escribiendo directamente al disco.
- Token Harbor (`tokenharbor.ai`) es un proveedor/gateway opcional. No es un
  agente ACP, no es una dependencia obligatoria y no debe desplazar los
  proveedores nativos del usuario.
- Nunca guardes tokens, claves o secretos en el repositorio. Usa referencias a
  variables de entorno o el almacén seguro previsto por la arquitectura.

## Implementación

- Usa `apply_patch` para cambios manuales en archivos.
- Conserva APIs genéricas y límites entre crates; evita concentrar lógica de
  protocolo, UI y persistencia en un mismo componente cuando pueda separarse.
- Añade pruebas proporcionales al cambio, especialmente para seguridad de
  paths, concurrencia de buffers, protocolo ACP/IPC y operaciones
  no destructivas.
- Marca casillas en los documentos de ejecución únicamente cuando el flujo
  correspondiente funcione de extremo a extremo.
- Si cambias tipos de configuración, regenera `docs/config.schema.json` con el
  generador del proyecto y verifica el test que compara el esquema.

## Validación obligatoria

Antes de dar una tarea por terminada ejecuta:

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
git diff --check
```

No ignores fallos ni reduzcas la severidad de Clippy para ocultar problemas.
Una excepción localizada solo es aceptable con una razón clara en el código.

## Git y entrega

- Crea commits locales pequeños, coherentes y con mensajes descriptivos.
- No hagas `push` salvo que el usuario lo solicite explícitamente.
- No reescribas, reviertas ni limpies cambios ajenos.
- Al entregar, informa qué se implementó, validaciones ejecutadas, hashes de
  commits y trabajo pendiente de la fase siguiente.
