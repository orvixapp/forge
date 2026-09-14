use crate::{
    AgentDefinition, ClientSurface, JsonRpcMessage, TransportError, read_jsonl, write_jsonl,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use thiserror::Error;
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::{Child, Command},
    sync::{Mutex, broadcast, oneshot},
    task::JoinHandle,
};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResult {
    #[serde(default)]
    pub protocol_version: Value,
    #[serde(default)]
    pub agent_capabilities: Value,
    #[serde(default)]
    pub auth_methods: Vec<Value>,
    #[serde(flatten)]
    pub extensions: HashMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcpSession {
    pub id: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(untagged)]
pub enum PromptBlock {
    Text { text: String },
    Raw(Value),
}

impl Serialize for PromptBlock {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Self::Text { text } => json!({"type": "text", "text": text}).serialize(serializer),
            Self::Raw(value) => value.serialize(serializer),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionMode {
    Plan,
    Edit,
    Auto,
}

#[derive(Debug, Clone, PartialEq)]
pub enum AcpEvent {
    SessionUpdate {
        session_id: Option<String>,
        update: Value,
    },
    Notification {
        method: String,
        params: Value,
    },
    Stderr(String),
    Disconnected,
}

#[derive(Debug, Error)]
pub enum ClientError {
    #[error("cannot spawn ACP agent: {0}")]
    Spawn(#[source] std::io::Error),
    #[error("ACP transport error: {0}")]
    Transport(#[from] TransportError),
    #[error("ACP process has no piped {0}")]
    MissingPipe(&'static str),
    #[error("ACP request channel closed")]
    Closed,
    #[error("ACP request failed ({code}): {message}")]
    Remote { code: i64, message: String },
    #[error("invalid ACP response to {method}: {message}")]
    InvalidResponse {
        method: &'static str,
        message: String,
    },
}

struct Shared {
    writer: Mutex<tokio::process::ChildStdin>,
    pending: Mutex<HashMap<u64, oneshot::Sender<Result<Value, ClientError>>>>,
    next_id: AtomicU64,
    events: broadcast::Sender<AcpEvent>,
}

/// Cloneable handle to a live ACP JSON-RPC connection.
#[derive(Clone)]
pub struct AcpClient {
    shared: Arc<Shared>,
}

impl AcpClient {
    async fn request(&self, method: &'static str, params: Value) -> Result<Value, ClientError> {
        let id = self.shared.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.shared.pending.lock().await.insert(id, tx);
        if let Err(error) = write_jsonl(
            &mut *self.shared.writer.lock().await,
            &JsonRpcMessage::request(id, method, params),
        )
        .await
        {
            self.shared.pending.lock().await.remove(&id);
            return Err(error.into());
        }
        rx.await.map_err(|_| ClientError::Closed)?
    }

    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<AcpEvent> {
        self.shared.events.subscribe()
    }

    /// # Errors
    /// Returns transport, remote JSON-RPC, or response-shape errors.
    pub async fn initialize(
        &self,
        client_name: &str,
        version: &str,
    ) -> Result<InitializeResult, ClientError> {
        let value = self.request("initialize", json!({"protocolVersion": 1, "clientInfo": {"name": client_name, "version": version}, "clientCapabilities": {"fs": {"readTextFile": true, "writeTextFile": true}, "terminal": true}})).await?;
        serde_json::from_value(value).map_err(|error| ClientError::InvalidResponse {
            method: "initialize",
            message: error.to_string(),
        })
    }

    /// # Errors
    /// Returns transport errors or an authentication rejection.
    pub async fn authenticate(&self, method_id: &str) -> Result<Value, ClientError> {
        self.request("authenticate", json!({"methodId": method_id}))
            .await
    }

    /// # Errors
    /// Returns transport, remote JSON-RPC, or response-shape errors.
    pub async fn new_session(
        &self,
        cwd: &Path,
        mcp_servers: Vec<Value>,
    ) -> Result<AcpSession, ClientError> {
        let value = self
            .request(
                "session/new",
                json!({"cwd": cwd, "mcpServers": mcp_servers}),
            )
            .await?;
        session_from(&value, "session/new")
    }

    /// # Errors
    /// Returns transport, remote JSON-RPC, or response-shape errors.
    pub async fn load_session(
        &self,
        session_id: &str,
        cwd: &Path,
        mcp_servers: Vec<Value>,
    ) -> Result<AcpSession, ClientError> {
        let value = self
            .request(
                "session/load",
                json!({"sessionId": session_id, "cwd": cwd, "mcpServers": mcp_servers}),
            )
            .await?;
        session_from(&value, "session/load")
    }

    /// # Errors
    /// Returns a transport error or a rejection from the agent.
    pub async fn prompt(
        &self,
        session: &AcpSession,
        prompt: Vec<PromptBlock>,
    ) -> Result<Value, ClientError> {
        self.request(
            "session/prompt",
            json!({"sessionId": session.id, "prompt": prompt}),
        )
        .await
    }

    /// # Errors
    /// Returns a transport error when the notification cannot be sent.
    pub async fn cancel(&self, session: &AcpSession) -> Result<(), ClientError> {
        self.notify("session/cancel", json!({"sessionId": session.id}))
            .await
    }

    /// # Errors
    /// Returns a transport error or a rejection from the agent.
    pub async fn set_mode(
        &self,
        session: &AcpSession,
        mode: SessionMode,
    ) -> Result<Value, ClientError> {
        self.request(
            "session/set_mode",
            json!({"sessionId": session.id, "modeId": mode}),
        )
        .await
    }

    /// # Errors
    /// Returns a transport error when the notification cannot be sent.
    pub async fn notify(&self, method: &str, params: Value) -> Result<(), ClientError> {
        write_jsonl(
            &mut *self.shared.writer.lock().await,
            &JsonRpcMessage::notification(method, params),
        )
        .await?;
        Ok(())
    }
}

fn session_from(value: &Value, method: &'static str) -> Result<AcpSession, ClientError> {
    value
        .get("sessionId")
        .and_then(Value::as_str)
        .map(|id| AcpSession { id: id.into() })
        .ok_or_else(|| ClientError::InvalidResponse {
            method,
            message: "missing sessionId".into(),
        })
}

/// Owns the child lifetime while [`AcpClient`] owns the protocol connection.
pub struct AgentProcess {
    child: Child,
    pub client: AcpClient,
    reader: JoinHandle<()>,
    stderr: JoinHandle<()>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum AgentProcessEvent {
    Started,
    Exited(Option<i32>),
    Restarting { attempt: u32, delay: Duration },
    Failed(String),
}

impl AgentProcess {
    /// Starts an adapter with piped stdio and the configured environment.
    ///
    /// # Errors
    /// Returns process-spawn and missing-pipe errors.
    pub fn spawn(
        definition: &AgentDefinition,
        surface: Arc<ClientSurface>,
    ) -> Result<Self, ClientError> {
        let mut command = Command::new(&definition.command);
        command
            .args(&definition.args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        if let Some(cwd) = &definition.cwd {
            command.current_dir(cwd);
        }
        command.envs(&definition.env);
        let mut child = command.spawn().map_err(ClientError::Spawn)?;
        let stdin = child
            .stdin
            .take()
            .ok_or(ClientError::MissingPipe("stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or(ClientError::MissingPipe("stdout"))?;
        let stderr_pipe = child
            .stderr
            .take()
            .ok_or(ClientError::MissingPipe("stderr"))?;
        let (events, _) = broadcast::channel(256);
        let shared = Arc::new(Shared {
            writer: Mutex::new(stdin),
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            events,
        });
        let reader_shared = Arc::clone(&shared);
        let reader = tokio::spawn(async move {
            let mut stdout = BufReader::new(stdout);
            loop {
                if let Ok(message) = read_jsonl(&mut stdout).await {
                    dispatch(message, &reader_shared, &surface).await;
                } else {
                    let _ = reader_shared.events.send(AcpEvent::Disconnected);
                    break;
                }
            }
            for (_, sender) in reader_shared.pending.lock().await.drain() {
                let _ = sender.send(Err(ClientError::Closed));
            }
        });
        let stderr_events = shared.events.clone();
        let stderr = tokio::spawn(async move {
            let mut lines = BufReader::new(stderr_pipe).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let _ = stderr_events.send(AcpEvent::Stderr(line));
            }
        });
        Ok(Self {
            child,
            client: AcpClient { shared },
            reader,
            stderr,
        })
    }

    /// # Errors
    /// Returns an operating-system error while waiting for the child.
    pub async fn wait(&mut self) -> Result<std::process::ExitStatus, std::io::Error> {
        self.child.wait().await
    }
}

impl Drop for AgentProcess {
    fn drop(&mut self) {
        self.reader.abort();
        self.stderr.abort();
    }
}

async fn dispatch(message: JsonRpcMessage, shared: &Shared, surface: &ClientSurface) {
    if let Some(id) = message.id.as_ref().and_then(Value::as_u64)
        && message.method.is_none()
        && let Some(sender) = shared.pending.lock().await.remove(&id)
    {
        let result = match message.error {
            Some(error) => Err(ClientError::Remote {
                code: error.get("code").and_then(Value::as_i64).unwrap_or(-32000),
                message: error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown ACP error")
                    .into(),
            }),
            None => Ok(message.result.unwrap_or(Value::Null)),
        };
        let _ = sender.send(result);
        return;
    }
    if message.id.is_some() {
        if let Some(response) = surface.handle_request(&message) {
            let _ = write_jsonl(&mut *shared.writer.lock().await, &response).await;
        }
        return;
    }
    if let Some(method) = message.method {
        let params = message.params.unwrap_or(Value::Null);
        let event = if method == "session/update" {
            AcpEvent::SessionUpdate {
                session_id: params
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                update: params
                    .get("update")
                    .cloned()
                    .unwrap_or_else(|| params.clone()),
            }
        } else {
            AcpEvent::Notification { method, params }
        };
        let _ = shared.events.send(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tolerates_initialize_extensions() {
        let parsed: InitializeResult = serde_json::from_value(json!({"protocol_version": 1, "agent_capabilities": {"loadSession": true}, "vendor": "forge"})).unwrap();
        assert_eq!(parsed.extensions["vendor"], "forge");
    }

    #[test]
    fn extracts_session_id() {
        assert_eq!(
            session_from(&json!({"sessionId": "s-1"}), "session/new")
                .unwrap()
                .id,
            "s-1"
        );
    }

    #[test]
    fn text_prompt_blocks_include_the_acp_discriminator() {
        assert_eq!(
            serde_json::to_value(PromptBlock::Text {
                text: "hola".into()
            })
            .unwrap(),
            json!({"type": "text", "text": "hola"})
        );
    }
}
