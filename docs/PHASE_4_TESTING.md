# Prueba manual de Fase 4

La implementación 4.1–4.3 ya es utilizable. Hace falta un agente que hable ACP;
Token Harbor es opcional y solamente cambia el proveedor de modelos que usa ese
agente.

## Camino corto con OpenCode

En esta máquina `opencode` ya está instalado. Añadir al archivo de usuario
`~/.config/forge/config.toml` (no al workspace):

```toml
[[agents]]
name = "OpenCode"
command = "opencode"
args = ["acp"]
enabled = true
```

Después:

1. Ejecutar `cargo run -p forge-gui` desde el workspace que se quiere abrir.
2. Pulsar `Ctrl+Shift+A`, o ejecutar `agent.newSession` desde `Ctrl+Shift+P`.
3. Enviar un prompt que pida leer un archivo abierto y ejecutar un comando
   corto, por ejemplo: `Lee @README.md y ejecuta printf forge-acp`.
4. Verificar que la respuesta llega en streaming y que la herramienta aparece
   en la línea de tiempo.
5. Hacer un cambio sin guardar en `README.md` y pedir que lea ese archivo. El
   agente debe recibir el texto del buffer, no la versión del disco.
6. Pedir una edición. Debe aparecer como propuesta y no modificar el archivo ni
   el buffer directamente.
7. Cuando el agente ejecute el comando, debe abrirse una pestaña
   `Agente · printf`; `terminal/output` devuelve su salida y la pestaña permanece
   visible al terminar. `terminal/release` la cierra.

La revisión visual y aceptación por hunk todavía pertenece a 4.4. Por eso una
edición puede probarse como propuesta no destructiva, pero aún no aceptarse por
hunks desde la UI.

## Token Harbor como proveedor opcional

El dominio correcto es `tokenharbor.ai`. Token Harbor no sustituye ACP ni se
añade como `[[agents]]`: se conecta al agente que Forge ya lanza.

1. Instalar y ejecutar el conector siguiendo la documentación oficial de Token
   Harbor.
2. Elegir solamente `opencode`, `codex` o `claude`, según el agente configurado
   en Forge. La clave se introduce en el prompt del conector; nunca se guarda en
   `config.toml` ni se pasa como argumento de proceso.
3. Comprobar la conexión con `tokenharbor status` y, si hace falta,
   `tokenharbor doctor`.
4. Reiniciar Forge para que el agente lea su configuración actualizada y repetir
   la prueba anterior.
5. Para volver al proveedor nativo, ejecutar `tokenharbor disconnect opencode`
   (o el nombre del agente seleccionado).

El conector oficial añade un provider a Codex y OpenCode, y configura las
variables del gateway para Claude Code. Esto mantiene a Forge independiente del
servicio: si Token Harbor no está instalado o conectado, el agente usa su
proveedor nativo.

## Otros agentes ACP

El registro oficial incluye, entre otros, Codex, Claude Agent, Gemini CLI y
OpenCode. Forge detecta ejecutables ya instalados en `PATH`; no descarga agentes
automáticamente. También se puede declarar cualquier adaptador explícitamente
con `[[agents]]`, indicando su comando ACP y sus argumentos.
