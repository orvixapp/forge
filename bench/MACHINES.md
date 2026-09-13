# Benchmark machines

Results are invalid until the exact hardware, OS, kernel, display stack and
power profile are recorded here.

| Profile | Hardware | OS / display | Purpose |
|---|---|---|---|
| R1 | TODO | TODO | Typical 16 GB laptop |
| R2 | TODO | TODO | 16+ core workstation |
| R3 | TODO | TODO | 4 core / 8 GB degraded mode |
| DEV-1 | AMD Ryzen 3 5300U (4c/8t, 2.6 GHz boost), 14 GiB RAM, Radeon integrated (RADV Renoir), NVMe | Linux 7.0.0-31-generic, KDE Plasma 6.6.6 on Wayland, Mesa 26.0.8 / Vulkan 1.4, internal 1366×768 @ 60 Hz plus a second display, CPU governor `powersave` (not fixed) | Development laptop where the Phase 0/1 numbers in `results/` were taken. Closest to R3; not a reference machine because the governor is dynamic and the desktop is in use |
| CI | GitHub-hosted `ubuntu-latest` (2 vCPU, no GPU), Xvfb 1600×900, Mesa lavapipe | Ubuntu, X11 via Xvfb, software Vulkan | Regression gate only (`thresholds.ci.toml`); never compared with R1–R3 |

## Method (ARCHITECTURE.md §5.2)

N = 10 runs, report median and p95, machine otherwise idle, fixed CPU
governor, same corpus commit. GUI scenarios must run one at a time: a window
opening or closing on top counts as focus changes and redraws.
