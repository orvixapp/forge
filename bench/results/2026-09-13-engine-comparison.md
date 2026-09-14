# 2026-09-13 — Ghostty vs Alacritty detrás de `VtEngine`

`proto-termd` acepta `--engine alacritty` cuando se compila con
`--features alacritty` (`crates/proto-termd/src/alacritty.rs`). Los tres
escenarios del daemon se ejecutaron con cada motor, mismo binario y misma
sesión de trabajo, cambiando sólo `FORGE_BENCH_ENGINE`:

```bash
cargo build --release -p proto-termd -p forge-bench --features proto-termd/alacritty
FORGE_BENCH_ENGINE=ghostty   target/release/forge-bench key_echo --iterations 200
FORGE_BENCH_ENGINE=alacritty target/release/forge-bench key_echo --iterations 200
```

## Máquina DEV-1 (ver `bench/MACHINES.md`), con carga de fondo

Durante la medida la máquina tenía 4 GiB de swap en uso y ~45 % de
`iowait` (otras aplicaciones abiertas), así que los valores absolutos no son
comparables con `2026-09-13-key-echo.md`; la comparación relativa entre
motores sí, porque ambos corrieron en las mismas condiciones y de forma
alterna.

| Escenario | Ghostty | Alacritty | Umbral |
|---|---|---|---|
| `key_echo` mediana / p95 (n = 200) | 0,27 / 0,42 ms | 0,54 / 0,70 ms | p95 ≤ 8 ms |
| `flood_input` mediana / p95 (n = 30, 1 GiB de salida) | 10,3 / 14,6 ms | 12,5 / 17,1 ms | p95 ≤ 16 ms |
| `termd_idle` PSS con 50 sesiones | 10,8 MiB (≈ 216 KiB/sesión) | 14,9 MiB (≈ 298 KiB/sesión) | ≤ 100 MiB |

## Lectura

- Ghostty gana en las tres métricas: la mitad de latencia tecla→eco, ~15 %
  menos de latencia bajo inundación y ~28 % menos memoria por sesión. Con
  la carga de fondo, Alacritty roza el umbral de `flood_input`.
- El adaptador de Alacritty pasa la misma suite de integración
  (`FORGE_TEST_ENGINE=alacritty`): salida, estilos, scrollback de 10k
  líneas, reattach, búsqueda, teclado, ratón y OSC 52. Le faltan las marcas
  OSC 133 y OSC 7 (no las implementa `alacritty_terminal`) y el teclado
  Kitty; para el producto se mantiene **Ghostty [DECIDIDO]** y el adaptador
  queda como oráculo de comparación y plan B.
- Pendiente: `vtebench` contra Alacritty en R1 (§29) cuando la máquina de
  referencia esté registrada; en DEV-1 no tiene sentido con swap activo.

## GUI en la misma sesión (orientativo)

`startup_empty` 124 ms mediana / 140 ms p95 (1 frame, 37,6 MiB);
`idle` 0 frames en 60 s; `panes_20` 3,0 / 3,2 ms; `grid_full` 4,5–4,7 /
6,7–7,1 ms; `ipc_round_trip` 0,03 / 0,20 ms. Los dos últimos superan los
valores registrados en `2026-09-13-grid-full.md` (3,5 / 4,7 y 2,05 / 2,67)
por la carga de fondo descrita arriba; deben repetirse con la máquina
ociosa antes de tomarlos como regresión.
