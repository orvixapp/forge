# Conformidad VT: `vttest` y `esctest`

Fase 2 exige "`vttest` en las secciones soportadas" (ARCHITECTURE.md §29).
El motor VT es `libghostty-vt`, que trae su propia suite; lo que se valida
aquí es la cadena completa Forge: GUI → daemon → PTY → Ghostty → render.

## vttest (interactivo)

```bash
sudo apt install vttest        # Debian/Ubuntu; en Arch: pacman -S vttest
cargo run --release            # abre Forge
vttest                         # dentro de Forge
```

Secciones a recorrer y resultado esperado en Forge:

| Menú | Sección | Esperado |
|---|---|---|
| 1 | Test of cursor movements | Rejillas y bordes alineados; sin celdas fuera de sitio al final de línea (autowrap). |
| 2 | Test of screen features | Modos 132/80 columnas se ignoran (Forge no redimensiona por DECCOLM); scroll region, origin mode, inverse y blink correctos. |
| 3 | Test of character sets | ASCII y DEC Special Graphics (líneas); los demás juegos nacionales no son objetivo. |
| 5 | Test of keyboard | Cursor keys (normal/application), función F1–F12, teclado numérico; en el modo de teclado Kitty Forge responde con CSI u. |
| 6 | Test of terminal reports | DA primario/secundario, DSR y cursor position report responden (es lo que usa `key_echo`). |
| 8 | Test of VT102 features | Insert/delete line y char, DECSTBM. |
| 11 | Test of non-VT100 features | 11.1 xterm: 256 colores, cursor styles (DECSCUSR), bracketed paste (11.6), mouse (11.7: normal, button-event, any-event, SGR). |

No aplican (Forge no los implementa): impresora (7), doble ancho/alto (2.x
DECDWL/DECDHL se muestran sin escalar), Sixel/ReGIS (11.x gráficos).

## esctest (automatizado)

`esctest` (de iTerm2) ejerce secuencias y comprueba las respuestas leyendo
la pantalla vía DECRQCRA, que Ghostty implementa:

```bash
git clone https://github.com/ThomasDickey/esctest2 ~/src/esctest
cd ~/src/esctest/esctest
# dentro de Forge:
python3 esctest.py --expected-terminal=xterm --xterm-checksum=334 \
  --max-vt-level=4 --logfile=/tmp/esctest.log \
  --include='(CUP|CUB|CUF|CUU|CUD|DECSTBM|ED|EL|ICH|DCH|IL|DL|SGR|DECSCUSR|DECRQM|XTERM_WINOPS|BracketedPaste)'
```

Los fallos esperados están en las secuencias que Ghostty declara no
soportadas (`GHOSTTY_TERMINAL_OPT_UNKNOWN_SEQUENCE` los reporta en el log
del daemon con `FORGE_LOG=debug`). Cualquier fallo en cursor, borrado,
inserción, SGR o modos es un bug del daemon/GUI y se abre como issue.

## Comparación con Alacritty como oráculo

El daemon compilado con `--features alacritty` corre la misma suite de
integración con el segundo motor (`FORGE_TEST_ENGINE=alacritty`), lo que
detecta divergencias de parseo entre motores antes de llegar a vttest:

```bash
FORGE_GHOSTTY_LIB=$PWD/target/ghostty/lib/libghostty-vt.so \
FORGE_TEST_ENGINE=alacritty \
cargo test -p proto-termd --features alacritty --test terminal_round_trip
```

## Registro

| Fecha | Máquina | vttest secciones OK | esctest | Notas |
|---|---|---|---|---|
| pendiente | DEV-1 | — | — | Se rellena en la validación manual de cierre de Fase 2 (bloque 1). |
