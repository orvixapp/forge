//! The MCP server proper: JSON-RPC over stdio, tools backed by the GUI.

use crate::PROTOCOL_VERSION;
use crate::bridge::BridgeClient;
use serde_json::{Value, json};
use std::fmt::Write as _;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Longest a `forge_run_in_terminal` command may run.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(120);
/// Output kept per command, so a runaway build does not flood the agent.
const OUTPUT_CAP: usize = 64 * 1024;

#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// GUI socket (`FORGE_GUI_SOCKET`); `None` runs without editor state.
    pub socket: Option<PathBuf>,
    /// Agent tab that spawned us (`FORGE_AGENT_TAB`).
    pub tab: u64,
    /// Session directory (`FORGE_WORKSPACE`, else the current directory).
    pub workspace: PathBuf,
}

impl ServerConfig {
    #[must_use]
    pub fn from_env() -> Self {
        Self {
            socket: std::env::var_os("FORGE_GUI_SOCKET").map(PathBuf::from),
            tab: std::env::var("FORGE_AGENT_TAB")
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(0),
            workspace: std::env::var_os("FORGE_WORKSPACE")
                .map(PathBuf::from)
                .or_else(|| std::env::current_dir().ok())
                .unwrap_or_else(|| PathBuf::from(".")),
        }
    }
}

/// Serves MCP on stdin/stdout until the agent closes the pipe.
///
/// # Errors
/// Only when stdout cannot be written.
pub fn serve_stdio(config: ServerConfig) -> io::Result<()> {
    let mut server = Server::new(config);
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut out = stdout.lock();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let message: Value = match serde_json::from_str(&line) {
            Ok(message) => message,
            Err(error) => {
                let reply = json!({"jsonrpc": "2.0", "id": Value::Null,
                    "error": {"code": -32700, "message": format!("parse error: {error}")}});
                writeln!(out, "{reply}")?;
                out.flush()?;
                continue;
            }
        };
        if let Some(reply) = server.handle(&message) {
            writeln!(out, "{reply}")?;
            out.flush()?;
        }
    }
    Ok(())
}

pub struct Server {
    config: ServerConfig,
    bridge: Option<BridgeClient>,
}

impl Server {
    #[must_use]
    pub fn new(config: ServerConfig) -> Self {
        let bridge = config
            .socket
            .as_deref()
            .and_then(|socket| BridgeClient::connect(socket, config.tab).ok());
        Self { config, bridge }
    }

    /// Handles one JSON-RPC message; notifications produce no reply.
    #[must_use]
    pub fn handle(&mut self, message: &Value) -> Option<Value> {
        let method = message.get("method").and_then(Value::as_str)?;
        let id = message.get("id").cloned();
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        let result = match method {
            "initialize" => Ok(json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {"tools": {}, "resources": {}},
                "serverInfo": {"name": "forge", "version": env!("CARGO_PKG_VERSION")},
                "instructions": "Forge exposes the editor: open buffers (unsaved content included), git status, a terminal that asks the user before running, and proposed edits that the user reviews hunk by hunk.",
            })),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({"tools": tool_definitions()})),
            "tools/call" => self.call_tool(&params),
            "resources/list" => Ok(self.list_resources()),
            "resources/read" => self.read_resource(&params),
            _ if method.starts_with("notifications/") => return None,
            _ => Err((-32601, format!("método desconocido: {method}"))),
        };
        // Notifications have no id and get no reply, even on error.
        let id = id?;
        Some(match result {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err((code, message)) => {
                json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
            }
        })
    }

    fn call_tool(&mut self, params: &Value) -> Result<Value, (i64, String)> {
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| (-32602, "se requiere name".to_owned()))?;
        let arguments = params.get("arguments").cloned().unwrap_or(json!({}));
        let outcome = match name {
            "forge_list_open_files" => self.list_open_files(),
            "forge_read_buffer" => self.read_buffer(&arguments),
            "forge_git_status" => self.git_status(),
            "forge_run_in_terminal" => self.run_in_terminal(&arguments),
            "forge_propose_edit" => self.propose_edit(&arguments),
            _ => return Err((-32602, format!("herramienta desconocida: {name}"))),
        };
        Ok(match outcome {
            Ok(text) => json!({"content": [{"type": "text", "text": text}], "isError": false}),
            Err(error) => json!({"content": [{"type": "text", "text": error}], "isError": true}),
        })
    }

    fn gui(&mut self, method: &str, params: Value) -> Result<Value, String> {
        match self.bridge.as_mut() {
            Some(bridge) => bridge.call(method, params),
            None => Err("Forge no está en ejecución (sin FORGE_GUI_SOCKET)".into()),
        }
    }

    fn list_open_files(&mut self) -> Result<String, String> {
        let files = self.gui("forge/list_open_files", json!({}))?;
        serde_json::to_string_pretty(&files).map_err(|error| error.to_string())
    }

    fn absolute(&self, arguments: &Value) -> Result<PathBuf, String> {
        let path = arguments
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| "se requiere path".to_owned())?;
        let path = Path::new(path);
        Ok(if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.config.workspace.join(path)
        })
    }

    fn read_buffer(&mut self, arguments: &Value) -> Result<String, String> {
        let path = self.absolute(arguments)?;
        if self.bridge.is_some() {
            let mut params = json!({"path": path, "sessionId": "mcp"});
            for key in ["line", "limit"] {
                if let Some(value) = arguments.get(key) {
                    params[key] = value.clone();
                }
            }
            let result = self.gui("fs/read_text_file", params)?;
            return result
                .get("content")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| "respuesta sin content".to_owned());
        }
        std::fs::read_to_string(&path).map_err(|error| format!("{}: {error}", path.display()))
    }

    fn git_status(&self) -> Result<String, String> {
        let output = std::process::Command::new("git")
            .args(["status", "--porcelain=v1", "--branch"])
            .current_dir(&self.config.workspace)
            .output()
            .map_err(|error| format!("git no disponible: {error}"))?;
        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).into_owned())
        } else {
            Err(String::from_utf8_lossy(&output.stderr).trim().to_owned())
        }
    }

    /// Asks the user through the GUI's permission card, then runs the
    /// command in the session directory and returns its output.
    fn run_in_terminal(&mut self, arguments: &Value) -> Result<String, String> {
        let command = arguments
            .get("command")
            .and_then(Value::as_str)
            .filter(|command| !command.trim().is_empty())
            .ok_or_else(|| "se requiere command".to_owned())?
            .to_owned();
        let answer = self.gui(
            "session/request_permission",
            json!({
                "sessionId": "mcp",
                "toolCall": {
                    "toolCallId": format!("mcp-run-{}", std::process::id()),
                    "title": format!("Ejecutar: {command}"),
                    "kind": "execute",
                    "name": "forge_run_in_terminal",
                    "arguments": {"command": command},
                },
                "options": [
                    {"optionId": "allow_once", "name": "Permitir una vez", "kind": "allow_once"},
                    {"optionId": "allow_always", "name": "Permitir siempre", "kind": "allow_always"},
                    {"optionId": "reject_once", "name": "Rechazar", "kind": "reject_once"},
                    {"optionId": "reject_always", "name": "Rechazar siempre", "kind": "reject_always"},
                ],
            }),
        )?;
        if !permission_granted(&answer) {
            return Err("el usuario no autorizó el comando".into());
        }
        run_command(&command, &self.config.workspace)
    }

    fn propose_edit(&mut self, arguments: &Value) -> Result<String, String> {
        let path = self.absolute(arguments)?;
        let content = arguments
            .get("content")
            .and_then(Value::as_str)
            .ok_or_else(|| "se requiere content".to_owned())?;
        self.gui(
            "fs/write_text_file",
            json!({"path": path, "content": content, "sessionId": "mcp"}),
        )?;
        Ok(format!(
            "Propuesta registrada para {}; el usuario la revisa hunk a hunk en Forge",
            path.display()
        ))
    }

    fn list_resources(&mut self) -> Value {
        let files = self
            .gui("forge/list_open_files", json!({}))
            .unwrap_or(json!({"files": []}));
        let resources: Vec<Value> = files
            .get("files")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|file| file.get("path").and_then(Value::as_str))
            .map(|path| {
                json!({
                    "uri": format!("forge://buffer{path}"),
                    "name": Path::new(path).file_name().map_or(path, |n| n.to_str().unwrap_or(path)),
                    "mimeType": "text/plain",
                })
            })
            .collect();
        json!({"resources": resources})
    }

    fn read_resource(&mut self, params: &Value) -> Result<Value, (i64, String)> {
        let uri = params
            .get("uri")
            .and_then(Value::as_str)
            .ok_or_else(|| (-32602, "se requiere uri".to_owned()))?;
        let path = uri
            .strip_prefix("forge://buffer")
            .ok_or_else(|| (-32602, format!("uri no soportada: {uri}")))?;
        let text = self
            .read_buffer(&json!({"path": path}))
            .map_err(|error| (-32603, error))?;
        Ok(json!({"contents": [{"uri": uri, "mimeType": "text/plain", "text": text}]}))
    }
}

fn permission_granted(answer: &Value) -> bool {
    answer
        .pointer("/outcome/optionId")
        .and_then(Value::as_str)
        .is_some_and(|option| option.starts_with("allow"))
        && answer.pointer("/outcome/outcome").and_then(Value::as_str) == Some("selected")
}

fn run_command(command: &str, cwd: &Path) -> Result<String, String> {
    let mut child = std::process::Command::new("sh")
        .args(["-lc", command])
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|error| format!("no se pudo lanzar sh: {error}"))?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let reader = |pipe: Option<std::process::ChildStdout>| {
        std::thread::spawn(move || {
            let mut text = String::new();
            if let Some(mut pipe) = pipe {
                let _ = io::Read::read_to_string(&mut pipe, &mut text);
            }
            text
        })
    };
    let out = reader(stdout);
    let err = std::thread::spawn(move || {
        let mut text = String::new();
        if let Some(mut pipe) = stderr {
            let _ = io::Read::read_to_string(&mut pipe, &mut text);
        }
        text
    });
    let started = std::time::Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if started.elapsed() > COMMAND_TIMEOUT => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(error) => return Err(error.to_string()),
        }
    };
    let mut stdout = out.join().unwrap_or_default();
    let mut stderr = err.join().unwrap_or_default();
    truncate_output(&mut stdout);
    truncate_output(&mut stderr);
    let mut report = format!("$ {command}\n");
    match status {
        Some(status) => {
            let _ = writeln!(report, "exit: {}", status.code().unwrap_or(-1));
        }
        None => {
            let _ = writeln!(report, "exit: killed after {}s", COMMAND_TIMEOUT.as_secs());
        }
    }
    if !stdout.is_empty() {
        report.push_str("--- stdout ---\n");
        report.push_str(&stdout);
    }
    if !stderr.is_empty() {
        if !report.ends_with('\n') {
            report.push('\n');
        }
        report.push_str("--- stderr ---\n");
        report.push_str(&stderr);
    }
    Ok(report)
}

fn truncate_output(text: &mut String) {
    if text.len() > OUTPUT_CAP {
        let mut cut = OUTPUT_CAP;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
        text.push_str("\n… (salida truncada)\n");
    }
}

/// Tools advertised in `tools/list`.
#[must_use]
pub fn tool_definitions() -> Vec<Value> {
    vec![
        json!({
            "name": "forge_list_open_files",
            "description": "Files open in the Forge editor with their dirty state, language and line count.",
            "inputSchema": {"type": "object", "properties": {}, "additionalProperties": false},
        }),
        json!({
            "name": "forge_read_buffer",
            "description": "Contents of a file as the editor sees it, unsaved changes included. Optional 1-based line and limit.",
            "inputSchema": {"type": "object", "required": ["path"], "properties": {
                "path": {"type": "string", "description": "Absolute path or relative to the workspace"},
                "line": {"type": "integer", "minimum": 1},
                "limit": {"type": "integer", "minimum": 1}
            }},
        }),
        json!({
            "name": "forge_git_status",
            "description": "`git status --porcelain --branch` of the session workspace.",
            "inputSchema": {"type": "object", "properties": {}, "additionalProperties": false},
        }),
        json!({
            "name": "forge_run_in_terminal",
            "description": "Runs a shell command in the workspace after the user approves it in Forge; returns exit code, stdout and stderr (120 s limit).",
            "inputSchema": {"type": "object", "required": ["command"], "properties": {
                "command": {"type": "string"}
            }},
        }),
        json!({
            "name": "forge_propose_edit",
            "description": "Proposes the full new content of a file. Nothing is written: the user reviews the diff hunk by hunk in Forge.",
            "inputSchema": {"type": "object", "required": ["path", "content"], "properties": {
                "path": {"type": "string"},
                "content": {"type": "string"}
            }},
        }),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn offline_server(workspace: &Path) -> Server {
        Server::new(ServerConfig {
            socket: None,
            tab: 0,
            workspace: workspace.to_path_buf(),
        })
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("forge-mcp-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn initialize_and_list_tools() {
        let dir = scratch("init");
        let mut server = offline_server(&dir);
        let reply = server
            .handle(&json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}))
            .unwrap();
        assert_eq!(reply["result"]["protocolVersion"], PROTOCOL_VERSION);
        assert!(
            server
                .handle(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
                .is_none()
        );
        let tools = server
            .handle(&json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}))
            .unwrap();
        let names: Vec<&str> = tools["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            [
                "forge_list_open_files",
                "forge_read_buffer",
                "forge_git_status",
                "forge_run_in_terminal",
                "forge_propose_edit"
            ]
        );
        for name in names {
            assert!(name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'));
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn read_buffer_falls_back_to_disk_without_gui() {
        let dir = scratch("read");
        std::fs::write(dir.join("a.txt"), "hola\n").unwrap();
        let mut server = offline_server(&dir);
        let reply = server
            .handle(&json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call",
                "params": {"name": "forge_read_buffer", "arguments": {"path": "a.txt"}}}))
            .unwrap();
        assert_eq!(reply["result"]["isError"], false);
        assert_eq!(reply["result"]["content"][0]["text"], "hola\n");
        let reply = server
            .handle(&json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call",
                "params": {"name": "forge_list_open_files"}}))
            .unwrap();
        assert_eq!(reply["result"]["isError"], true);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn run_in_terminal_requires_permission_from_the_gui() {
        let dir = scratch("run");
        let mut server = offline_server(&dir);
        let reply = server
            .handle(&json!({"jsonrpc": "2.0", "id": 5, "method": "tools/call",
                "params": {"name": "forge_run_in_terminal", "arguments": {"command": "echo hi"}}}))
            .unwrap();
        assert_eq!(reply["result"]["isError"], true);
        assert!(permission_granted(
            &json!({"outcome": {"outcome": "selected", "optionId": "allow_once"}})
        ));
        assert!(!permission_granted(
            &json!({"outcome": {"outcome": "selected", "optionId": "reject_once"}})
        ));
        assert!(!permission_granted(
            &json!({"outcome": {"outcome": "cancelled"}})
        ));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn commands_report_exit_code_and_both_streams() {
        let dir = scratch("cmd");
        let report = run_command("echo out; echo err >&2; exit 3", &dir).unwrap();
        assert!(report.contains("exit: 3"));
        assert!(report.contains("--- stdout ---\nout\n"));
        assert!(report.contains("--- stderr ---\nerr\n"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn unknown_methods_and_tools_are_errors() {
        let dir = scratch("unknown");
        let mut server = offline_server(&dir);
        let reply = server
            .handle(&json!({"jsonrpc": "2.0", "id": 6, "method": "nope"}))
            .unwrap();
        assert_eq!(reply["error"]["code"], -32601);
        let reply = server
            .handle(&json!({"jsonrpc": "2.0", "id": 7, "method": "tools/call",
                "params": {"name": "forge_fly"}}))
            .unwrap();
        assert_eq!(reply["error"]["code"], -32602);
        let _ = std::fs::remove_dir_all(dir);
    }
}
