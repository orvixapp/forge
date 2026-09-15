//! User-facing text in one language at a time (`ui.language`).
//!
//! Source strings are English; [`tr`] returns the Spanish rendering when
//! the configured language is Spanish and the table knows the string, and
//! the English text otherwise, so a missing translation shows up as
//! English rather than as a hole. Protocol errors sent to agents (ACP, MCP)
//! and log lines stay English and never go through here.

use crate::config::Language;
use std::fmt::Display;
use std::sync::atomic::{AtomicBool, Ordering};

static ENGLISH: AtomicBool = AtomicBool::new(false);

pub fn set_language(language: Language) {
    ENGLISH.store(language == Language::English, Ordering::Relaxed);
}

#[must_use]
pub fn language() -> Language {
    if ENGLISH.load(Ordering::Relaxed) {
        Language::English
    } else {
        Language::Spanish
    }
}

/// The configured rendering of an English source string.
#[must_use]
pub fn tr(english: &'static str) -> &'static str {
    if ENGLISH.load(Ordering::Relaxed) {
        english
    } else {
        spanish(english).unwrap_or(english)
    }
}

/// [`tr`] for templates: each `{}` takes the next argument.
#[must_use]
pub fn trf(english: &'static str, args: &[&dyn Display]) -> String {
    let template = tr(english);
    let mut out = String::with_capacity(template.len() + args.len() * 8);
    let mut rest = template;
    for arg in args {
        match rest.split_once("{}") {
            Some((head, tail)) => {
                out.push_str(head);
                out.push_str(&arg.to_string());
                rest = tail;
            }
            None => break,
        }
    }
    out.push_str(rest);
    out
}

#[allow(clippy::too_many_lines)]
fn spanish(english: &str) -> Option<&'static str> {
    Some(match english {
        // ----- commands (shell.rs titles) -------------------------------
        "New terminal tab" => "Nueva pestaña de terminal",
        "New terminal tab in directory…" => "Nueva pestaña de terminal en un directorio…",
        "Close tab or window" => "Cerrar pestaña o ventana",
        "Toggle maximized window" => "Maximizar/restaurar la ventana",
        "Show command palette" => "Mostrar la paleta de comandos",
        "Split pane horizontally" => "Dividir el panel en horizontal",
        "Split pane vertically" => "Dividir el panel en vertical",
        "Focus next pane" => "Ir al panel siguiente",
        "Focus previous pane" => "Ir al panel anterior",
        "Show process explorer" => "Mostrar el explorador de procesos",
        "Cycle color theme" => "Cambiar al siguiente tema de color",
        "Reload configuration file" => "Recargar el archivo de configuración",
        "Search terminal scrollback" => "Buscar en el historial de la terminal",
        "Find next occurrence" => "Buscar la siguiente coincidencia",
        "Find previous occurrence" => "Buscar la coincidencia anterior",
        "Jump to previous shell prompt" => "Ir al prompt anterior",
        "Jump to next shell prompt" => "Ir al prompt siguiente",
        "Send SIGINT to foreground process" => "Enviar SIGINT al proceso en primer plano",
        "Send SIGTERM to foreground process" => "Enviar SIGTERM al proceso en primer plano",
        "Send SIGKILL to foreground process" => "Enviar SIGKILL al proceso en primer plano",
        "Rename active tab…" => "Renombrar la pestaña activa…",
        "Move active tab left" => "Mover la pestaña activa a la izquierda",
        "Move active tab right" => "Mover la pestaña activa a la derecha",
        "Increase font size" => "Aumentar el tamaño de fuente",
        "Decrease font size" => "Reducir el tamaño de fuente",
        "Reset font size" => "Restablecer el tamaño de fuente",
        "Toggle active pane zoom" => "Ampliar/restaurar el panel activo",
        "Close other split panes" => "Cerrar los demás paneles",
        "New terminal with profile…" => "Nueva terminal con perfil…",
        "Open file…" => "Abrir archivo…",
        "New untitled file" => "Nuevo archivo sin título",
        "Save active file" => "Guardar el archivo activo",
        "Undo last edit" => "Deshacer",
        "Redo undone edit" => "Rehacer",
        "Select all" => "Seleccionar todo",
        "Copy selection" => "Copiar la selección",
        "Cut selection" => "Cortar la selección",
        "Paste into editor" => "Pegar en el editor",
        "Add cursor at next match" => "Añadir cursor en la siguiente coincidencia",
        "Add cursor on line above" => "Añadir cursor en la línea superior",
        "Add cursor on line below" => "Añadir cursor en la línea inferior",
        "Find file in project…" => "Buscar archivo en el proyecto…",
        "Search text in project…" => "Buscar texto en el proyecto…",
        "Find in current file" => "Buscar en el archivo actual",
        "Find and replace in file" => "Buscar y reemplazar en el archivo",
        "Load large file into memory for editing" => {
            "Cargar el archivo grande en memoria para editarlo"
        }
        "Toggle word wrap" => "Activar/desactivar el ajuste de línea",
        "Toggle minimap" => "Mostrar/ocultar el minimapa",
        "Show context menu" => "Mostrar el menú contextual",
        "Paste clipboard" => "Pegar el portapapeles",
        "New agent session" => "Nueva sesión de agente",
        "Ask the agent about the selection…" => "Preguntar al agente sobre la selección…",
        "Investigate the last failed command with the agent" => {
            "Investigar el último comando fallido con el agente"
        }
        "Forward the last prompt to another provider…" => {
            "Reenviar el último prompt a otro proveedor…"
        }
        "Accept all proposed hunks" => "Aceptar todos los hunks propuestos",
        "Reject all proposed hunks" => "Rechazar todos los hunks propuestos",
        "Settings…" => "Ajustes…",
        "Close tab" => "Cerrar pestaña",
        // ----- settings menu ---------------------------------------------
        "Settings" => "Ajustes",
        "Show symbol documentation" => "Mostrar documentación del símbolo",
        "Go to definition" => "Ir a definición",
        "Show signature help" => "Mostrar ayuda de firma",
        "Show language diagnostics" => "Mostrar diagnósticos del lenguaje",
        "Language server" => "Servidor de lenguaje",
        "Diagnostics (up to 1000)" => "Diagnósticos (hasta 1000)",
        "Format on save: on (turn off)" => "Formatear al guardar: activado (desactivar)",
        "Format on save: off (turn on)" => "Formatear al guardar: desactivado (activar)",
        "Format on save enabled" => "Formatear al guardar activado",
        "Format on save disabled" => "Formatear al guardar desactivado",
        "Find references" => "Buscar referencias",
        "Go to implementation" => "Ir a la implementación",
        "Rename symbol" => "Renombrar símbolo",
        "Code actions and quick fixes" => "Acciones de código y correcciones rápidas",
        "Format document" => "Formatear documento",
        "Code actions" => "Acciones de código",
        "No code actions here" => "No hay acciones de código aquí",
        "No locations found" => "No se encontraron ubicaciones",
        "{} locations" => "{} ubicaciones",
        "No language server for this file" => "No hay servidor de lenguaje para este archivo",
        "Language server {} is not installed (`{}` not found in PATH); the local completion stays. Install it or change [[languages]] in the user config." => {
            "El servidor de lenguaje {} no está instalado (`{}` no está en el PATH); se mantiene el completado local. Instálalo o cambia [[languages]] en la configuración de usuario."
        }
        "Nothing to change" => "Nada que cambiar",
        "Apply {} edits in {} files" => "Aplicar {} cambios en {} archivos",
        "{} · {} edits" => "{} · {} cambios",
        "{}: {} edits in {} files" => "{}: {} cambios en {} archivos",
        "{}: could not edit {}" => "{}: no se pudo editar {}",
        "Rename to {}" => "Renombrar a {}",
        "{} needs a server command, which Forge does not run" => {
            "{} requiere un comando del servidor, que Forge no ejecuta"
        }
        "Place the cursor on a diagnostic to investigate it" => {
            "Coloca el cursor sobre un diagnóstico para investigarlo"
        }
        "agent.investigate works from a terminal or a diagnostic" => {
            "agent.investigate se usa desde una terminal o un diagnóstico"
        }
        "Symbol documentation (LSP hover)" => "Documentación del símbolo (hover LSP)",
        "The language server reports a problem at {}:{}. Investigate the cause and propose a fix.\n\n" => {
            "El servidor de lenguaje informa de un problema en {}:{}. Investiga la causa y propón una corrección.\n\n"
        }
        "The excerpt comes from the unsaved buffer; the git diff only covers what is on disk." => {
            "El fragmento procede del buffer sin guardar; el git diff solo cubre lo que hay en disco."
        }
        "Esc closes" => "Esc cierra",
        "Open user settings (config.toml)" => "Abrir ajustes de usuario (config.toml)",
        "Open workspace settings (.forge/config.toml)" => {
            "Abrir ajustes del workspace (.forge/config.toml)"
        }
        "Add provider…" => "Añadir proveedor…",
        "Add agent…" => "Añadir agente…",
        "Import MCP servers…" => "Importar servidores MCP…",
        "Manage MCP servers…" => "Gestionar servidores MCP…",
        "Add HTTP MCP server…" => "Añadir servidor MCP HTTP…",
        "MCP server name" => "Nombre del servidor MCP",
        "MCP HTTPS URL (OAuth is authorized separately in each agent)" => {
            "URL HTTPS del MCP (OAuth se autoriza por separado en cada agente)"
        }
        "Invalid or duplicate MCP server — use HTTPS and a unique name" => {
            "Servidor MCP inválido o duplicado — usa HTTPS y un nombre único"
        }
        "Import MCP definitions only — no credentials" => {
            "Importar solo definiciones MCP — sin credenciales"
        }
        "MCP configuration file from {}" => "Archivo de configuración MCP de {}",
        "MCP: {} imported (disabled), {} duplicates, {} skipped. Review in settings." => {
            "MCP: {} importados (desactivados), {} duplicados, {} omitidos. Revisar en ajustes."
        }
        "MCP import warnings — no credentials copied" => {
            "Avisos de importación MCP — sin copiar credenciales"
        }
        "All agents" => "Todos los agentes",
        "Enabled" => "Activado",
        "Disabled" => "Desactivado",
        "Edit MCP definitions in user settings…" => {
            "Editar definiciones MCP en ajustes de usuario…"
        }
        "MCP servers — changes apply to new sessions" => {
            "Servidores MCP — cambios para sesiones nuevas"
        }
        "Disable server" => "Desactivar servidor",
        "Enable server (starts a process or connects remotely)" => {
            "Activar servidor (inicia un proceso o conecta remotamente)"
        }
        "Choose allowed agents…" => "Elegir agentes permitidos…",
        "Allowed agent names, comma separated (empty = all)" => {
            "Nombres de agentes separados por comas (vacío = todos)"
        }
        "Unknown agent name — add it in settings first" => {
            "Agente desconocido — añádelo primero en ajustes"
        }
        "MCP settings saved — open a new agent session" => {
            "Ajustes MCP guardados — abre una sesión nueva del agente"
        }
        "MCP server no longer exists — reopen settings" => {
            "El servidor MCP ya no existe — vuelve a abrir ajustes"
        }
        "Save or discard unsaved settings before using a wizard" => {
            "Guarda o descarta los ajustes sin guardar antes de usar el asistente"
        }
        "Cannot read user settings" => "No se pueden leer los ajustes de usuario",
        "Choose theme…" => "Elegir tema…",
        "Language: Español" => "Idioma: Español",
        "Language: English" => "Idioma: English",
        "Switch to English" => "Cambiar a English",
        "Switch to Español" => "Cambiar a Español",
        "Provider name (e.g. gpt, local)" => "Nombre del proveedor (p. ej. gpt, local)",
        "Provider kind" => "Tipo de proveedor",
        "Model (e.g. gpt-5, claude-opus-5, qwen2.5-coder)" => {
            "Modelo (p. ej. gpt-5, claude-opus-5, qwen2.5-coder)"
        }
        "Base URL (empty = the provider's default)" => "URL base (vacío = la del proveedor)",
        "Environment variable holding the API key (empty = none)" => {
            "Variable de entorno con la clave de API (vacío = ninguna)"
        }
        "Use this provider for" => "Usar este proveedor para",
        "Only add it" => "Solo añadirlo",
        "All task classes (router)" => "Todas las clases de tarea (router)",
        "Agent name (e.g. opencode, codex)" => "Nombre del agente (p. ej. opencode, codex)",
        "Command (e.g. opencode)" => "Comando (p. ej. opencode)",
        "Arguments, space separated (e.g. acp)" => "Argumentos separados por espacios (p. ej. acp)",
        "Provider for this agent" => "Proveedor para este agente",
        "None (the agent's own login)" => "Ninguno (el login del propio agente)",
        "Provider {} added to {}" => "Proveedor {} añadido a {}",
        "Agent {} added to {}" => "Agente {} añadido a {}",
        "Could not write {}: {}" => "No se pudo escribir {}: {}",
        "No user config path (benchmark mode)" => {
            "Sin ruta de configuración de usuario (modo benchmark)"
        }
        "Enter continues · Esc cancels" => "Enter continúa · Esc cancela",
        "Language: {}" => "Idioma: {}",
        // ----- notifications and statuses (window.rs) --------------------
        "Invalid configuration: {}" => "Configuración inválida: {}",
        "Configuration reloaded" => "Configuración recargada",
        "Configuration unchanged" => "Configuración sin cambios",
        "Theme: {}" => "Tema: {}",
        "Could not save the session: {}" => "No se pudo guardar la sesión: {}",
        "Saved session is invalid and was ignored: {}" => "Sesión guardada inválida, se ignora: {}",
        "benchmark → no provider" => "benchmark → sin proveedor",
        "the agent's own provider" => "el proveedor del agente",
        "Provider {} is not defined in [[providers]]; the agent's own is used" => {
            "Proveedor {} no definido en [[providers]]; se usa el del agente"
        }
        "Session in worktree {} (branch {})" => "Sesión en worktree {} (rama {})",
        "Could not create the worktree; the session uses the current directory: {}" => {
            "No se pudo crear el worktree; la sesión usa el directorio actual: {}"
        }
        "git is not available for worktrees ({}); using the current directory" => {
            "git no disponible para worktrees ({}); se usa el directorio actual"
        }
        "Select code (or open a file) to ask the agent" => {
            "Selecciona código (o abre un archivo) para preguntar al agente"
        }
        "Ask the agent about {}" => "Pregunta al agente sobre {}",
        "agent.investigate works from a terminal" => "agent.investigate se usa desde una terminal",
        "No prompt marks (shell integration); sending the visible screen" => {
            "Sin marcas de prompt (integración de shell); se envía la pantalla visible"
        }
        "The last command in the terminal failed or did not do what was expected. Investigate the cause and propose a fix.\n\nDirectory: {}\nCommand: {}\n" => {
            "El último comando en la terminal falló o no hizo lo esperado. Investiga la causa y propón la solución.\n\nDirectorio: {}\nComando: {}\n"
        }
        "agent.forward works from an agent session" => {
            "agent.forward se usa desde una sesión de agente"
        }
        "There is no prompt to forward yet" => "Todavía no hay un prompt que reenviar",
        "No [[providers]] configured to forward to" => {
            "Sin [[providers]] configurados para reenviar"
        }
        "Forward to provider" => "Reenviar a proveedor",
        "Connecting to forge-termd…" => "Conectando a forge-termd…",
        "Empty pane" => "Panel vacío",
        "Close “{}” without saving?" => "¿Cerrar «{}» sin guardar?",
        "Changes will be lost; the journal keeps a copy until the next save." => {
            "Los cambios se perderán; el journal conserva una copia hasta el próximo guardado."
        }
        "Save and close" => "Guardar y cerrar",
        "Discard" => "Descartar",
        "Cancel" => "Cancelar",
        "Close" => "Cerrar",
        "Agent process finished" => "Proceso del agente terminado",
        "Agent process exited with code {}" => "Proceso del agente terminó con código {}",
        "The shell exited with code {}" => "La shell terminó con código {}",
        "Daemon error: {}" => "Error del daemon: {}",
        "Allow once" => "Permitir una vez",
        "Allow for this session" => "Permitir en esta sesión",
        "Allow always" => "Permitir siempre",
        "Reject" => "Rechazar",
        "Permission required · {}" => "Permiso requerido · {}",
        "{} requests authorization" => "{} solicita autorización",
        "{} requests authorization for: {}" => "{} solicita autorización para: {}",
        "Proposed edit · {}" => "Edición propuesta · {}",
        "{} proposed hunks; awaiting review" => "{} hunks propuestos; pendiente de revisión",
        "The proposed change was not applied: {}" => "No se aplicó el cambio propuesto: {}",
        "Could not open {} to apply the proposal" => {
            "No se pudo abrir {} para aplicar la propuesta"
        }
        "The proposed changes were not applied: {}" => "No se aplicaron los cambios propuestos: {}",
        "{} conflicting hunks were left unapplied; review them one by one" => {
            "{} hunks en conflicto quedan sin aplicar; revísalos uno a uno"
        }
        "Session active" => "Sesión activa",
        "Session active · revision {}" => "Sesión activa · revisión {}",
        "Invalid patch: {}" => "Patch inválido: {}",
        "Cancellation requested" => "Cancelación solicitada",
        "Forwarded to {} ({} → new session)" => "Reenviado a {} ({} → nueva sesión)",
        "Tab name" => "Nombre de la pestaña",
        "No profiles: add [[profiles]] with name/shell/args/cwd/env to config.toml" => {
            "Sin perfiles: añade [[profiles]] con name/shell/args/cwd/env en config.toml"
        }
        "Terminal profile" => "Perfil de terminal",
        "default shell" => "shell por defecto",
        "Signal sent: {}" => "Señal enviada: {}",
        "A program tried to write the clipboard (OSC 52); denied by configuration" => {
            "Un programa intentó escribir el portapapeles (OSC 52); denegado por configuración"
        }
        "A terminal program" => "Un programa de la terminal",
        "Allow writing the clipboard?" => "¿Permitir escribir el portapapeles?",
        "{} wants to copy {} characters: {}" => "{} quiere copiar {} caracteres: {}",
        "Could not run {}: {}" => "No se pudo ejecutar {}: {}",
        "Open terminal in…" => "Abrir terminal en…",
        "Close the Forge window?" => "¿Cerrar la ventana de Forge?",
        "The terminal session stays alive in forge-termd." => {
            "La sesión de terminal sigue viva en forge-termd."
        }
        "The text has {} lines and the application does not use bracketed paste: every line break will run as Enter." => {
            "El texto tiene {} líneas y la aplicación no usa bracketed paste: cada salto de línea se ejecutará como Enter."
        }
        "The text contains the end sequence of bracketed paste (ESC [201~), which can inject commands." => {
            "El texto contiene la secuencia de fin de bracketed paste (ESC [201~), que puede inyectar comandos."
        }
        "Paste anyway?" => "¿Pegar de todas formas?",
        // ----- chrome.rs -------------------------------------------------
        "{} · new tab" => "{} · nueva pestaña",
        "theme {} · font {} {}px · config {}" => "tema {} · fuente {} {}px · config {}",
        "{} lines" => "{} líneas",
        "{} events" => "{} eventos",
        "ACP session · {} · {}" => "Sesión ACP · {} · {}",
        "no id" => "sin id",
        "Route: {}" => "Ruta: {}",
        "Type a prompt…  (Enter sends)" => "Escribe un prompt…  (Enter para enviar)",
        "events {}–{} of {} · wheel/PageUp/PageDown to navigate" => {
            "eventos {}–{} de {} · rueda/PageUp/PageDown para navegar"
        }
        "Permission request · {}" => "Solicitud de permiso · {}",
        "The agent asks to run {}" => "El agente solicita ejecutar {}",
        "Target: {}" => "Objetivo: {}",
        "Accept" => "Aceptar",
        "Accept all" => "Aceptar todo",
        "Reject all" => "Rechazar todo",
        "Conflict: {}" => "Conflicto: {}",
        "Hunk #{} · Lines {}-{}" => "Hunk #{} · Líneas {}-{}",
        "⚠ Conflict detected: concurrent changes in the buffer. Review them before accepting." => {
            "⚠ Conflicto detectado: modificaciones concurrentes en el buffer. Revisa los cambios antes de aceptar."
        }
        "You" => "Tú",
        "Agent" => "Agente",
        "System" => "Sistema",
        "Tool" => "Herramienta",
        "pending" => "pendiente",
        "running" => "en curso",
        "completed" => "completada",
        "failed" => "falló",
        "waiting for permission" => "espera permiso",
        "Enter applies · empty restores the automatic name · Esc cancels" => {
            "Enter aplica · vacío recupera el nombre automático · Esc cancela"
        }
        "↑↓ select · Enter opens · Esc closes" => "↑↓ selecciona · Enter abre · Esc cierra",
        "Go to file" => "Ir a archivo",
        "Search in project" => "Buscar en el proyecto",
        "↑↓ select · Enter opens · Alt+R regex · Alt+C case · Alt+W word · Esc closes" => {
            "↑↓ selecciona · Enter abre · Alt+R regex · Alt+C mayúsculas · Alt+W palabra · Esc cierra"
        }
        "Enter next · Shift+Enter previous · Tab field · Ctrl+Enter replace · Ctrl+Alt+Enter all · Alt+R/C/W" => {
            "Enter siguiente · Shift+Enter anterior · Tab campo · Ctrl+Enter reemplaza · Ctrl+Alt+Enter todos · Alt+R/C/W"
        }
        "Enter/Y allow · A allow for this tab · Esc/N deny" => {
            "Enter/Y permitir · A permitir en esta pestaña · Esc/N denegar"
        }
        "Enter/Y paste · Esc/N cancel" => "Enter/Y pegar · Esc/N cancelar",
        "type to search" => "escribe para buscar",
        "no matches" => "sin coincidencias",
        "Enter/↑ older · Shift+Enter/↓ newer · Alt+R regex · Alt+C case · Esc closes" => {
            "Enter/↑ anterior · Shift+Enter/↓ siguiente · Alt+R regex · Alt+C mayúsculas · Esc cierra"
        }
        "↑↓ select · Enter runs · Esc closes" => "↑↓ selecciona · Enter ejecuta · Esc cierra",
        "Type to filter commands" => "Escribe para filtrar comandos",
        // ----- editor.rs / project.rs ------------------------------------
        "the file takes {} MiB and only {} MiB are available; not loading it into memory" => {
            "el archivo ocupa {} MiB y sólo hay {} MiB disponibles; no se carga en memoria"
        }
        "recovered from the journal" => "recuperado del journal",
        "● unsaved" => "● sin guardar",
        "Untitled" => "Sin título",
        "{} opened in large-file mode (read-only, no highlighting)" => {
            "{} abierto en modo archivo grande (solo lectura, sin resaltado)"
        }
        "Could not map {}: {}" => "No se pudo mapear {}: {}",
        "Could not open {}: {}" => "No se pudo abrir {}: {}",
        "{} contains undecodable bytes" => "{} contiene bytes no decodificables",
        "{} recovered from the journal; save to keep the changes" => {
            "{} recuperado del journal; guarda para conservar los cambios"
        }
        "File loaded into memory; it can be edited now" => {
            "Archivo cargado en memoria; ya se puede editar"
        }
        "No journal for {}: {}" => "Sin journal para {}: {}",
        "Could not save: {}" => "No se pudo guardar: {}",
        "untitled.txt" => "sin-titulo.txt",
        "Open file" => "Abrir archivo",
        "Large file is read-only: use editor.materialize to edit it" => {
            "Archivo grande en solo lectura: usa editor.materialize para editarlo"
        }
        "Edit failed: {}" => "Edición fallida: {}",
        "{} files" => "{} archivos",
        "indexing…" => "indexando…",
        "{}{} results · {} files" => "{}{} resultados · {} archivos",
        "type to search the project" => "escribe para buscar en el proyecto",
        "{} results…" => "{} resultados…",
        "{} was deleted on disk; save to recreate it" => {
            "{} se borró en disco; guarda para recrearlo"
        }
        "{} changed on disk; reopen it to see the new content" => {
            "{} cambió en disco; reábrelo para ver el contenido nuevo"
        }
        "{} changed on disk and has unsaved changes" => {
            "{} cambió en disco y tiene cambios sin guardar"
        }
        // ----- ipc.rs / agent.rs statuses --------------------------------
        "No connection: {}" => "Sin conexión: {}",
        "forge-termd is not available on this platform yet" => {
            "forge-termd no está disponible en esta plataforma todavía"
        }
        "Session {} recovered" => "Sesión {} recuperada",
        "Session {} connected" => "Sesión {} conectada",
        "Ready to start a session" => "Lista para iniciar sesión",
        "Permission granted ({})" => "Permiso concedido ({})",
        "Permission denied by the user" => "Permiso denegado por el usuario",
        "Waiting for the agent…" => "Esperando al agente…",
        "Receiving the answer…" => "Recibiendo respuesta…",
        "Ready" => "Listo",
        "ACP disconnected" => "ACP desconectado",
        "Thinking…" => "Pensando…",
        "● Thinking…" => "● Pensando…",
        "Reasoning" => "Razonamiento",
        "The agent is working…" => "El agente está trabajando…",
        "Ask about the code or request a change…" => {
            "Pregunta sobre el código o solicita un cambio…"
        }
        "Esc stop" => "Esc detener",
        "Enter send · Shift+Enter newline" => "Enter enviar · Shift+Enter nueva línea",
        "… {} earlier lines\n{}" => "… {} líneas anteriores\n{}",
        "{}\n… {} more lines" => "{}\n… {} líneas más",
        "No ACP adapter found in PATH" => "No se encontró ningún adaptador ACP instalado en PATH",
        "Could not create the ACP runtime" => "No se pudo crear el runtime ACP",
        "ACP could not start: {}" => "ACP no pudo arrancar: {}",
        "ACP initialize failed: {}" => "ACP initialize falló: {}",
        "ACP skipped {} events" => "ACP omitió {} eventos",
        "Could not open the ACP session: {}" => "No se pudo abrir sesión ACP: {}",
        "ACP disconnected; retrying in {} ms" => "ACP desconectado; reintentando en {} ms",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn templates_fill_placeholders_in_order() {
        set_language(Language::English);
        assert_eq!(
            trf("Could not run {}: {}", &[&"ls", &"boom"]),
            "Could not run ls: boom"
        );
        assert_eq!(trf("plain", &[&1]), "plain");
        set_language(Language::Spanish);
        assert_eq!(trf("Theme: {}", &[&"forge-dark"]), "Tema: forge-dark");
        assert_eq!(tr("Accept"), "Aceptar");
        // Unknown strings fall back to English instead of vanishing.
        assert_eq!(tr("definitely not translated"), "definitely not translated");
    }

    #[test]
    fn every_command_title_has_a_spanish_rendering() {
        for command in crate::shell::ShellCommand::ALL {
            assert!(
                spanish(command.title()).is_some(),
                "missing Spanish for {:?}",
                command.title()
            );
        }
    }
}
