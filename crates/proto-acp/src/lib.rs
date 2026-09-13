use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};
use thiserror::Error;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JsonRpcMessage {
    pub jsonrpc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<Value>,
}

impl JsonRpcMessage {
    #[must_use]
    pub fn request(id: u64, method: impl Into<String>, params: Value) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id: Some(Value::from(id)),
            method: Some(method.into()),
            params: Some(params),
            result: None,
            error: None,
        }
    }

    #[must_use]
    pub fn response(id: Value, result: Value) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id: Some(id),
            method: None,
            params: None,
            result: Some(result),
            error: None,
        }
    }

    #[must_use]
    pub fn error(id: Option<Value>, code: i64, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            method: None,
            params: None,
            result: None,
            error: Some(serde_json::json!({"code": code, "message": message.into()})),
        }
    }
}

/// The small, auditable client surface required by the Phase 0 ACP spike.
/// Files are served from in-memory buffers first so an agent can inspect an
/// unsaved edit without gaining access outside the workspace.
#[derive(Debug, Clone)]
pub struct ClientSurface {
    workspace: PathBuf,
    buffers: HashMap<PathBuf, String>,
    permission: PermissionChoice,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionChoice {
    Allow,
    Deny,
}

impl ClientSurface {
    #[must_use]
    pub fn new(workspace: PathBuf, permission: PermissionChoice) -> Self {
        Self {
            workspace,
            buffers: HashMap::new(),
            permission,
        }
    }

    pub fn set_buffer(&mut self, path: PathBuf, content: String) {
        self.buffers.insert(path, content);
    }

    /// Handles only agent-to-client requests needed for the spike. Unknown
    /// methods receive the standard JSON-RPC method-not-found response.
    #[must_use]
    pub fn handle_request(&self, message: &JsonRpcMessage) -> Option<JsonRpcMessage> {
        let id = message.id.clone()?;
        let method = message.method.as_deref()?;
        match method {
            "fs/read_text_file" => Some(self.read_text_file(id, message.params.as_ref())),
            "session/request_permission" => {
                Some(self.request_permission(id, message.params.as_ref()))
            }
            _ => Some(JsonRpcMessage::error(
                Some(id),
                -32601,
                format!("unsupported ACP method {method}"),
            )),
        }
    }

    fn read_text_file(&self, id: Value, params: Option<&Value>) -> JsonRpcMessage {
        let Some(path) = params
            .and_then(|params| params.get("path"))
            .and_then(Value::as_str)
        else {
            return JsonRpcMessage::error(Some(id), -32602, "fs/read_text_file requires path");
        };
        let path = PathBuf::from(path);
        let Ok(path) = self.workspace_path(&path) else {
            return JsonRpcMessage::error(Some(id), -32001, "file path is outside the workspace");
        };
        let content = self
            .buffers
            .get(&path)
            .cloned()
            .or_else(|| std::fs::read_to_string(&path).ok());
        let Some(content) = content else {
            return JsonRpcMessage::error(
                Some(id),
                -32002,
                format!("cannot read {}", path.display()),
            );
        };
        let line = params
            .and_then(|params| params.get("line"))
            .and_then(Value::as_u64)
            .unwrap_or(1);
        let limit = params
            .and_then(|params| params.get("limit"))
            .and_then(Value::as_u64);
        let start = usize::try_from(line.saturating_sub(1)).unwrap_or(usize::MAX);
        let selected = content
            .lines()
            .skip(start)
            .take(
                limit
                    .and_then(|limit| usize::try_from(limit).ok())
                    .unwrap_or(usize::MAX),
            )
            .collect::<Vec<_>>()
            .join("\n");
        JsonRpcMessage::response(id, serde_json::json!({"content": selected}))
    }

    fn request_permission(&self, id: Value, params: Option<&Value>) -> JsonRpcMessage {
        let option_id = params
            .and_then(|params| params.get("options"))
            .and_then(Value::as_array)
            .and_then(|options| match self.permission {
                PermissionChoice::Allow => options.first(),
                PermissionChoice::Deny => options
                    .iter()
                    .find(|option| {
                        option
                            .get("kind")
                            .and_then(Value::as_str)
                            .is_some_and(|kind| kind.contains("deny"))
                    })
                    .or_else(|| options.last()),
            })
            .and_then(|option| option.get("optionId").or_else(|| option.get("id")))
            .cloned();
        JsonRpcMessage::response(
            id,
            serde_json::json!({"outcome": {"outcome": "selected", "optionId": option_id}}),
        )
    }

    fn workspace_path(&self, requested: &Path) -> Result<PathBuf, ()> {
        let workspace = self.workspace.canonicalize().map_err(|_| ())?;
        let path = requested.canonicalize().map_err(|_| ())?;
        path.starts_with(&workspace).then_some(path).ok_or(())
    }
}

#[derive(Debug, Error)]
pub enum TransportError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid JSON-RPC message: {0}")]
    Json(#[from] serde_json::Error),
    #[error("agent closed stdout")]
    EndOfStream,
}

/// Writes one JSON-RPC message using newline-delimited JSON framing.
///
/// # Errors
///
/// Returns an error when serialization or writing fails.
pub async fn write_jsonl<W: AsyncWrite + Unpin>(
    writer: &mut W,
    message: &JsonRpcMessage,
) -> Result<(), TransportError> {
    let mut encoded = serde_json::to_vec(message)?;
    encoded.push(b'\n');
    writer.write_all(&encoded).await?;
    writer.flush().await?;
    Ok(())
}

/// Reads one newline-delimited JSON-RPC message.
///
/// # Errors
///
/// Returns an error for invalid JSON, an I/O failure or a closed stream.
pub async fn read_jsonl<R: AsyncBufRead + Unpin>(
    reader: &mut R,
) -> Result<JsonRpcMessage, TransportError> {
    let mut line = String::new();
    if reader.read_line(&mut line).await? == 0 {
        return Err(TransportError::EndOfStream);
    }
    Ok(serde_json::from_str(&line)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::io::BufReader;

    #[tokio::test]
    async fn jsonl_round_trip() {
        let expected = JsonRpcMessage {
            jsonrpc: "2.0".into(),
            id: Some(json!(1)),
            method: Some("initialize".into()),
            params: Some(json!({"protocolVersion": 1})),
            result: None,
            error: None,
        };
        let (mut writer, reader) = tokio::io::duplex(1024);
        write_jsonl(&mut writer, &expected).await.unwrap();
        let mut reader = BufReader::new(reader);
        assert_eq!(read_jsonl(&mut reader).await.unwrap(), expected);
    }

    #[test]
    fn reads_an_unsaved_workspace_buffer_by_lines() {
        let root = std::env::temp_dir().join(format!("forge-acp-test-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("draft.rs");
        let mut surface = ClientSurface::new(root.clone(), PermissionChoice::Deny);
        surface.set_buffer(path.clone(), "one\ntwo\nthree".into());
        std::fs::write(&path, "old").unwrap();
        let response = surface
            .handle_request(&JsonRpcMessage::request(
                7,
                "fs/read_text_file",
                json!({"path": path, "line": 2, "limit": 1}),
            ))
            .unwrap();
        assert_eq!(response.result, Some(json!({"content": "two"})));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejects_a_file_outside_the_workspace() {
        let root = std::env::temp_dir().join(format!("forge-acp-test-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let outside = std::env::temp_dir().join("forge-acp-outside.txt");
        std::fs::write(&outside, "secret").unwrap();
        let surface = ClientSurface::new(root.clone(), PermissionChoice::Deny);
        let response = surface
            .handle_request(&JsonRpcMessage::request(
                8,
                "fs/read_text_file",
                json!({"path": outside}),
            ))
            .unwrap();
        assert_eq!(response.error.unwrap()["code"], -32001);
        std::fs::remove_file(outside).unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }
}
