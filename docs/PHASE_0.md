# Cierre documental de Fase 0

**Estado:** en progreso; este documento distingue evidencia registrada de
trabajo aún pendiente. No convierte un prototipo ni un escáner estático en una
decisión de producto.

## Propósito

Fase 0 reduce las apuestas de arquitectura antes de construir el producto. El
plan completo y los criterios técnicos siguen siendo la fuente normativa en
[`ARCHITECTURE.md`](ARCHITECTURE.md), especialmente §§5, 20, 29 y el apartado
“Primer prototipo”. Este archivo es el índice de evidencias y la lista de
cierre para que una afirmación futura pueda rastrearse hasta un comando,
máquina y artefacto.

## Estado de evidencias

| Apuesta | Evidencia actual | Estado | Cierre que falta |
|---|---|---|---|
| A — renderer GPUI | [`2026-09-13-grid-full.md`](../bench/results/2026-09-13-grid-full.md) mide el grid 200×60 enteramente sucio. | Parcial | Repetir en máquinas de referencia y decidir si el p95 cumple o se usa el fallback. |
| B — `forge-termd` | IPC, PTY, attach/detach y VT están en el prototipo. [`2026-09-13-key-echo.md`](../bench/results/2026-09-13-key-echo.md): tecla→patch 0,14 ms mediana / 0,2 ms p95. Reattach automatizado recupera 10.002 filas; [`2026-09-13-termd-idle-50.md`](../bench/results/2026-09-13-termd-idle-50.md): 7.878 KiB PSS con 50 sesiones. | Parcial (latencia, reattach y carga ✓) | Validar `kill -9` de la GUI real, `vtebench` y repetir puertas finales en R1. |
| C — ACP | Existen primitivas JSON-RPC/JSONL para el spike. | Parcial | Registrar la matriz de capacidades contra los tres agentes indicados en la arquitectura. |
| D — extensiones VS Code | `tools/vscode-api-scan` está implementado y documentado. | Preparado | Ejecutar y conservar un snapshot de Open VSX; priorizar con el CSV, sin llamar a ello compatibilidad. |
| Máquinas y baselines | [`bench/MACHINES.md`](../bench/MACHINES.md) define el formato. | Pendiente | Registrar R1/R2/R3 y publicar los baselines comparables. |

## Extensiones VS Code: evidencia y límites

Forge usa Open VSX y VSIX local; no usa el marketplace de Microsoft. El
escáner de Fase 0 recopila dos señales:

1. Referencias estáticas `vscode.namespace.member` dentro de archivos
   JavaScript de un VSIX.
2. Claves de primer nivel de `package.json#contributes`.

La salida ordena el trabajo por `installs`, pero no prueba que una extensión
active, ni que una API se implemente con la semántica correcta. En particular,
no detecta imports con alias, accesos computados, código generado ni el camino
ejecutado en tiempo de ejecución. Una extensión sin `main` ni `browser` es una
señal útil para Tier 0, no una garantía de que todas sus contribuciones sean
compatibles.

El procedimiento, formato CSV y requisitos están en
[`tools/vscode-api-scan/README.md`](../tools/vscode-api-scan/README.md).

## Criterio de aceptación del escaneo

Para marcar D como cerrado, el repositorio debe contener un directorio fechado
`bench/results/openvsx-YYYY-MM-DD/` con:

- `api-usage.csv`, producido con `--top 1000` o con el tamaño de muestra
  explícitamente declarado;
- `summary.json`, la salida JSON del comando, que incluye extensiones
  escaneadas, extensiones sin host y filas emitidas;
- la versión de Node y el SHA-256 del CSV, anotados junto al resultado;
- una nota corta que separe las contribuciones Tier 0 de los miembros API que
  alimentarán Tier 1, incluyendo las diez primeras filas de cada grupo;
- cualquier error o extensión que no pudo descargarse. Una ejecución parcial
  no se presenta como top-1.000.

Una vez exista ese snapshot, el siguiente paso es una tabla de decisión con
`miembro → tier objetivo → propietario → prueba de extensión real`. Solo una
prueba de conformidad puede cambiar el estado de una API a “soportada”.

## Convención para los próximos informes

Cada informe de `bench/results/` incluye fecha, commit, comando exacto,
máquina, muestras, resultado frente al umbral y la decisión que permite o
bloquea. Si un dato no se midió, se escribe “pendiente”; no se extrapola desde
otra máquina ni desde una referencia externa.
