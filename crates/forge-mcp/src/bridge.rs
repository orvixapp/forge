//! JSONL request/response link between `forge mcp-server` and the GUI.
//!
//! One line per message in both directions. Requests carry the agent tab
//! they belong to so the GUI can route permission cards and proposed edits
//! to the right session; the method names are the ACP client methods the
//! GUI already implements (`fs/read_text_file`, `session/request_permission`,
//! …) plus the `forge/*` queries that have no ACP equivalent.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BridgeRequest {
    pub id: u64,
    /// Agent tab that owns the MCP session (`FORGE_AGENT_TAB`), `0` when
    /// the server was started by hand.
    pub tab: u64,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BridgeResponse {
    pub id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Where the GUI listens for this process: `$XDG_RUNTIME_DIR` when set,
/// else the temp directory.
#[must_use]
pub fn default_socket_path(pid: u32) -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|dir| dir.is_dir())
        .unwrap_or_else(std::env::temp_dir)
        .join(format!("forge-gui-{pid}.sock"))
}

/// Client side (the MCP server process).
pub struct BridgeClient {
    reader: BufReader<std::os::unix::net::UnixStream>,
    writer: std::os::unix::net::UnixStream,
    next_id: u64,
    tab: u64,
}

impl BridgeClient {
    /// # Errors
    /// When the socket cannot be opened.
    pub fn connect(path: &Path, tab: u64) -> io::Result<Self> {
        let stream = std::os::unix::net::UnixStream::connect(path)?;
        stream.set_read_timeout(Some(std::time::Duration::from_secs(600)))?;
        let reader = BufReader::new(stream.try_clone()?);
        Ok(Self {
            reader,
            writer: stream,
            next_id: 1,
            tab,
        })
    }

    /// Sends one request and waits for its answer.
    ///
    /// # Errors
    /// I/O failures, a closed GUI, or an error answer from the GUI.
    pub fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id;
        self.next_id += 1;
        let request = BridgeRequest {
            id,
            tab: self.tab,
            method: method.to_owned(),
            params,
        };
        let mut line = serde_json::to_string(&request).map_err(|error| error.to_string())?;
        line.push('\n');
        self.writer
            .write_all(line.as_bytes())
            .map_err(|error| format!("GUI desconectada: {error}"))?;
        loop {
            let mut buffer = String::new();
            let read = self
                .reader
                .read_line(&mut buffer)
                .map_err(|error| format!("GUI desconectada: {error}"))?;
            if read == 0 {
                return Err("la GUI cerró el socket".into());
            }
            let response: BridgeResponse = match serde_json::from_str(buffer.trim_end()) {
                Ok(response) => response,
                Err(_) => continue,
            };
            if response.id != id {
                continue;
            }
            return match (response.result, response.error) {
                (_, Some(error)) => Err(error),
                (Some(result), None) => Ok(result),
                (None, None) => Ok(Value::Null),
            };
        }
    }
}

/// Server side (the GUI). Accepts connections on `path` and answers each
/// request with `handler`, one thread per connection so a tool waiting on a
/// permission card does not block the others.
///
/// # Errors
/// When the socket cannot be bound.
pub fn listen<F>(path: &Path, handler: F) -> io::Result<BridgeListener>
where
    F: Fn(BridgeRequest) -> Result<Value, String> + Send + Sync + 'static,
{
    let _ = std::fs::remove_file(path);
    let listener = std::os::unix::net::UnixListener::bind(path)?;
    let handler = std::sync::Arc::new(handler);
    let socket = path.to_path_buf();
    std::thread::Builder::new()
        .name("forge-mcp-bridge".into())
        .spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                let handler = handler.clone();
                let _ = std::thread::Builder::new()
                    .name("forge-mcp-conn".into())
                    .spawn(move || serve_connection(stream, handler.as_ref()));
            }
        })?;
    Ok(BridgeListener { socket })
}

fn serve_connection<F>(stream: std::os::unix::net::UnixStream, handler: &F)
where
    F: Fn(BridgeRequest) -> Result<Value, String>,
{
    let Ok(mut writer) = stream.try_clone() else {
        return;
    };
    let reader = BufReader::new(stream);
    for line in reader.lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(request) = serde_json::from_str::<BridgeRequest>(&line) else {
            continue;
        };
        let id = request.id;
        let response = match handler(request) {
            Ok(result) => BridgeResponse {
                id,
                result: Some(result),
                error: None,
            },
            Err(error) => BridgeResponse {
                id,
                result: None,
                error: Some(error),
            },
        };
        let Ok(mut text) = serde_json::to_string(&response) else {
            continue;
        };
        text.push('\n');
        if writer.write_all(text.as_bytes()).is_err() {
            break;
        }
    }
}

/// Removes the socket file when the GUI shuts down.
pub struct BridgeListener {
    socket: PathBuf,
}

impl BridgeListener {
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.socket
    }
}

impl Drop for BridgeListener {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.socket);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_round_trip_over_the_socket() {
        let dir = std::env::temp_dir().join(format!("forge-mcp-bridge-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("gui.sock");
        let listener = listen(&socket, |request| {
            if request.method == "fail" {
                Err("nope".into())
            } else {
                Ok(serde_json::json!({
                    "echo": request.params,
                    "tab": request.tab,
                    "method": request.method,
                }))
            }
        })
        .unwrap();
        let mut client = BridgeClient::connect(listener.path(), 7).unwrap();
        let reply = client
            .call("forge/list_open_files", serde_json::json!({"x": 1}))
            .unwrap();
        assert_eq!(reply["tab"], 7);
        assert_eq!(reply["method"], "forge/list_open_files");
        assert_eq!(reply["echo"]["x"], 1);
        assert_eq!(client.call("fail", Value::Null), Err("nope".into()));
        drop(listener);
        assert!(!socket.exists());
        let _ = std::fs::remove_dir_all(dir);
    }
}
