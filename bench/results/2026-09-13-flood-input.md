# 2026-09-13 — Entrada bajo salida sostenida de 1 GiB

```console
./target/release/forge-bench flood_input --iterations 50 --check
```

El escenario arma primero un lector de un byte dentro del PTY y luego inicia
un productor de 1 GiB. Sólo comienza a medir después de observar tanto la señal
de preparado como salida posterior del productor. La muestra termina cuando el
marcador generado al recibir el byte cruza PTY, parser, daemon y socket.

## Máquina DEV-1

| Métrica | Valor | Objetivo (§29 Fase 2) |
|---|---:|---:|
| mediana | 11,43 ms | — |
| p95 | 15,56 ms | ≤16 ms |
| muestras | 50 | — |

Resultado: **PASS**. Para evitar amplificar snapshots y eventos bajo salida
sostenida, el lector del PTY procesa bloques de 64 KiB; la escritura de input
continúa en un hilo dedicado y no espera al parser VT.

Esta evidencia corresponde a DEV-1. La puerta normativa todavía debe repetirse
en R1 junto con `vtebench`.
