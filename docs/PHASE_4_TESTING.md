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
6. Pedir una edición (`fs/write_text_file`). Debe aparecer como propuesta en el panel
   del agente y abrir el visor de diff con los hunks calculados mediante Myers diff.
   Verificar que no modifica el archivo en disco ni el buffer directamente.
7. En la tarjeta de revisión de diff:
   - Revisar cada hunk individualmente con sus líneas afectadas (`+` y `-`).
   - Aceptar un hunk con "Aceptar": se aplica como `Transaction` en el buffer del editor
     (admitiendo deshacer con `Ctrl+Z`) y el hunk pasa a estado "Aceptado".
   - Rechazar un hunk con "Rechazar": el hunk pasa a estado "Rechazado" sin modificar el buffer.
   - Usar "Aceptar todo" (`agent.acceptAllHunks`) o "Rechazar todo" (`agent.rejectAllHunks`)
     desde los botones o desde la paleta de comandos (`Ctrl+Shift+P`).
8. Si se modifica el buffer concurrentemente mientras hay una propuesta pendiente:
   - Los hunks no afectados se rebasan limpiamente recalculando offsets.
   - Los hunks cuyas líneas coincidan con la edición concurrente se marcan con
     `Conflicto: El contenido del buffer no coincide con la versión base`, impidiendo
     su aplicación hasta resolver el conflicto.
9. Cuando el agente solicita ejecutar comandos o herramientas que requieren permiso
   (`session/request_permission`):
   - Aparece una tarjeta de permiso en el panel del agente con botones:
     "Permitir una vez", "Permitir en esta sesión", "Permitir siempre", "Rechazar".
   - La solicitud espera la decisión del usuario antes de responder al agente.
   - "Permitir en esta sesión" recuerda la decisión para la herramienta/comando en la
     sesión activa sin volver a preguntar.
   - "Permitir siempre" persiste la regla en
     `~/.config/forge/permissions/<workspace>-<hash>.json` (nunca dentro del
     repositorio: un clon no puede traer permisos). Un "Permitir siempre"
     sobre `execute`/terminal/red sin comando concreto se aplica sólo una
     vez.
   - "Rechazar" responde negativamente al agente con la opción de denegación.
10. Cuando el agente ejecute el comando autorizado, se abre la pestaña
    `Agente · <comando>`; `terminal/output` devuelve su salida y la pestaña permanece
    visible al terminar. `terminal/release` la cierra.

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
