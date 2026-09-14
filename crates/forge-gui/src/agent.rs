//! Agent-session state rendered by the GUI. Protocol I/O stays in
//! `proto-acp`; this module is deliberately pure so streaming updates can be
//! batched and tested without a GPUI window.

use serde_json::Value;
use std::{
    future::Future,
    ops::Range,
    path::PathBuf,
    pin::Pin,
    sync::{Arc, mpsc::Sender},
    time::Duration,
};
use tokio::sync::mpsc as async_mpsc;

use crate::ipc::UiEvent;
use proto_acp::{
    AcpEvent, AgentDefinition, AgentProcess, AgentRegistry, ClientHandler, ClientSurface,
    JsonRpcMessage, PermissionChoice, PromptBlock,
};

struct ForgeClientBridge {
    tab_id: u64,
    events: Sender<UiEvent>,
    fallback: ClientSurface,
}

impl ClientHandler for ForgeClientBridge {
    fn handle<'a>(
        &'a self,
        message: &'a JsonRpcMessage,
    ) -> Pin<Box<dyn Future<Output = Option<JsonRpcMessage>> + Send + 'a>> {
        Box::pin(async move {
            match message.method.as_deref() {
                Some(method) if method.starts_with("fs/") || method.starts_with("terminal/") => {
                    let (response, receiver) = tokio::sync::oneshot::channel();
                    self.events
                        .send(UiEvent::AgentRequest {
                            tab_id: self.tab_id,
                            message: Box::new(message.clone()),
                            response,
                        })
                        .ok()?;
                    receiver.await.ok()
                }
                _ => self.fallback.handle(message).await,
            }
        })
    }
}

pub enum AgentCommand {
    Prompt(Vec<PromptBlock>),
    Cancel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageRole {
    User,
    Agent,
    System,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolState {
    Pending,
    Running,
    Succeeded,
    Failed,
    WaitingPermission,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TimelineItem {
    Message {
        role: MessageRole,
        text: String,
    },
    ToolCall {
        id: String,
        title: String,
        state: ToolState,
        detail: String,
    },
    Plan {
        title: String,
        entries: Vec<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptContext {
    pub label: String,
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProposedEdit {
    pub path: PathBuf,
    pub original: String,
    pub proposed: String,
}

pub struct AgentTab {
    pub agent_name: String,
    pub session_id: Option<String>,
    pub status: String,
    pub prompt: String,
    pub timeline: Vec<TimelineItem>,
    pub context: Vec<PromptContext>,
    pub proposed_edits: Vec<ProposedEdit>,
    pub scroll_item: usize,
    pub visible_items: usize,
    following_tail: bool,
    workspace: PathBuf,
    commands: Option<async_mpsc::UnboundedSender<AgentCommand>>,
}

impl AgentTab {
    #[must_use]
    pub fn new(agent_name: impl Into<String>, workspace: PathBuf) -> Self {
        let agent_name = agent_name.into();
        Self {
            status: "Lista para iniciar sesión".into(),
            agent_name,
            session_id: None,
            prompt: String::new(),
            timeline: Vec::new(),
            context: Vec::new(),
            proposed_edits: Vec::new(),
            scroll_item: 0,
            visible_items: 40,
            following_tail: true,
            workspace,
            commands: None,
        }
    }

    pub fn connect(&mut self, commands: async_mpsc::UnboundedSender<AgentCommand>) {
        self.commands = Some(commands);
        self.status = "Conectando…".into();
    }

    pub fn cancel(&self) {
        if let Some(commands) = &self.commands {
            let _ = commands.send(AgentCommand::Cancel);
        }
    }

    pub fn submit_prompt(&mut self) -> Option<String> {
        let prompt = self.prompt.trim().to_owned();
        if prompt.is_empty() {
            return None;
        }
        self.prompt.clear();
        self.resolve_file_mentions(&prompt);
        self.timeline.push(TimelineItem::Message {
            role: MessageRole::User,
            text: prompt.clone(),
        });
        self.status = "Esperando al agente…".into();
        if let Some(commands) = &self.commands {
            let mut blocks = vec![PromptBlock::Text {
                text: prompt.clone(),
            }];
            blocks.extend(self.context.iter().map(|context| {
                PromptBlock::Raw(serde_json::json!({
                    "type": "resource",
                    "resource": {"uri": context.label, "text": context.content}
                }))
            }));
            let _ = commands.send(AgentCommand::Prompt(blocks));
        }
        self.scroll_to_end();
        Some(prompt)
    }

    pub fn set_context(&mut self, context: Vec<PromptContext>) {
        self.context = context;
    }

    fn resolve_file_mentions(&mut self, prompt: &str) {
        for mention in prompt
            .split_whitespace()
            .filter_map(|word| word.strip_prefix('@'))
        {
            let mention = mention.trim_matches(|character: char| ",.;:)]}".contains(character));
            let requested = self.workspace.join(mention);
            let Ok(path) = requested.canonicalize() else {
                continue;
            };
            let Ok(workspace) = self.workspace.canonicalize() else {
                continue;
            };
            if !path.starts_with(&workspace)
                || self
                    .context
                    .iter()
                    .any(|item| item.label == path.display().to_string())
            {
                continue;
            }
            if let Ok(content) = std::fs::read_to_string(&path) {
                self.context.push(PromptContext {
                    label: path.display().to_string(),
                    content,
                });
            }
        }
    }

    pub fn scroll_by(&mut self, delta: isize) {
        let maximum = self.timeline.len().saturating_sub(self.visible_items);
        self.scroll_item = self.scroll_item.saturating_add_signed(delta).min(maximum);
        self.following_tail = self.scroll_item == maximum;
    }

    pub fn scroll_to_end(&mut self) {
        self.scroll_item = self.timeline.len().saturating_sub(self.visible_items);
        self.following_tail = true;
    }

    #[must_use]
    pub fn visible_range(&self) -> Range<usize> {
        let start = self.scroll_item.min(self.timeline.len());
        start..(start + self.visible_items).min(self.timeline.len())
    }

    /// Applies one tolerant `session/update`. Unknown update shapes are kept
    /// as system messages, making optional vendor capabilities visible rather
    /// than fatal to the session.
    pub fn apply_update(&mut self, update: &Value) {
        let follow_tail = self.following_tail;
        let kind = update
            .get("sessionUpdate")
            .or_else(|| update.get("type"))
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        match kind {
            "agent_message_chunk" | "message_chunk" => {
                let text = update
                    .get("content")
                    .and_then(|content| content.get("text"))
                    .or_else(|| update.get("text"))
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                self.append_agent_chunk(text);
                self.status = "Recibiendo respuesta…".into();
            }
            "tool_call" | "tool_call_update" => self.apply_tool_update(update),
            "plan" => {
                let entries = update
                    .get("entries")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|entry| {
                        entry
                            .get("content")
                            .or_else(|| entry.get("text"))
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                    })
                    .collect();
                self.timeline.push(TimelineItem::Plan {
                    title: update
                        .get("title")
                        .and_then(Value::as_str)
                        .unwrap_or("Plan")
                        .into(),
                    entries,
                });
            }
            _ => self.timeline.push(TimelineItem::Message {
                role: MessageRole::System,
                text: update.to_string(),
            }),
        }
        if follow_tail {
            self.scroll_to_end();
        }
    }

    fn append_agent_chunk(&mut self, chunk: &str) {
        if let Some(TimelineItem::Message {
            role: MessageRole::Agent,
            text,
        }) = self.timeline.last_mut()
        {
            text.push_str(chunk);
        } else {
            self.timeline.push(TimelineItem::Message {
                role: MessageRole::Agent,
                text: chunk.into(),
            });
        }
    }

    fn apply_tool_update(&mut self, update: &Value) {
        let id = update
            .get("toolCallId")
            .or_else(|| update.get("id"))
            .and_then(Value::as_str)
            .unwrap_or("tool")
            .to_owned();
        let state = match update.get("status").and_then(Value::as_str) {
            Some("in_progress" | "running") => ToolState::Running,
            Some("completed" | "succeeded") => ToolState::Succeeded,
            Some("failed") => ToolState::Failed,
            Some("waiting_permission") => ToolState::WaitingPermission,
            _ => ToolState::Pending,
        };
        let title = update
            .get("title")
            .or_else(|| update.get("name"))
            .and_then(Value::as_str)
            .unwrap_or("Herramienta")
            .to_owned();
        let detail = update
            .get("content")
            .or_else(|| update.get("detail"))
            .map(Value::to_string)
            .unwrap_or_default();
        if let Some(TimelineItem::ToolCall {
            title: old_title,
            state: old_state,
            detail: old_detail,
            ..
        }) = self
            .timeline
            .iter_mut()
            .find(|item| matches!(item, TimelineItem::ToolCall { id: old_id, .. } if old_id == &id))
        {
            *old_title = title;
            *old_state = state;
            *old_detail = detail;
        } else {
            self.timeline.push(TimelineItem::ToolCall {
                id,
                title,
                state,
                detail,
            });
        }
    }

    #[must_use]
    pub fn visible_timeline(&self, range: Range<usize>) -> &[TimelineItem] {
        let start = range.start.min(self.timeline.len());
        let end = range.end.min(self.timeline.len()).max(start);
        &self.timeline[start..end]
    }
}

pub fn spawn_agent_worker(
    definition: Option<AgentDefinition>,
    registry_cache: PathBuf,
    workspace: PathBuf,
    tab_id: u64,
    events: Sender<UiEvent>,
    mut commands: async_mpsc::UnboundedReceiver<AgentCommand>,
) {
    std::thread::Builder::new()
        .name(format!("forge-agent-{tab_id}"))
        .spawn(move || {
            let Some(definition) = definition.or_else(|| discover_agent(&registry_cache)) else {
                let _ = events.send(UiEvent::AgentStatus {
                    tab_id,
                    status: "No se encontró ningún adaptador ACP instalado en PATH".into(),
                });
                return;
            };
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build();
            let Ok(runtime) = runtime else {
                let _ = events.send(UiEvent::AgentStatus { tab_id, status: "No se pudo crear el runtime ACP".into() });
                return;
            };
            runtime.block_on(async move {
                let surface: Arc<dyn ClientHandler> = Arc::new(ForgeClientBridge {
                    tab_id,
                    events: events.clone(),
                    fallback: ClientSurface::new(workspace.clone(), PermissionChoice::Deny),
                });
                let mut previous_session = None;
                let mut attempt = 0_u32;
                loop {
                    let mut process = match AgentProcess::spawn(&definition, Arc::clone(&surface)) {
                        Ok(process) => process,
                        Err(error) => {
                            let _ = events.send(UiEvent::AgentStatus { tab_id, status: format!("ACP no pudo arrancar: {error}") });
                            return;
                        }
                    };
                    let client = process.client.clone();
                    let initialized = client.initialize("Forge", env!("CARGO_PKG_VERSION")).await;
                    if let Err(error) = initialized {
                        let _ = events.send(UiEvent::AgentStatus { tab_id, status: format!("ACP initialize falló: {error}") });
                    } else {
                        if let Some(method) = &definition.auth_method { let _ = client.authenticate(method).await; }
                        let session = match previous_session.as_deref() {
                            Some(id) => match client.load_session(id, &workspace, Vec::new()).await {
                                Ok(session) => Ok(session),
                                Err(_) => client.new_session(&workspace, Vec::new()).await,
                            },
                            None => client.new_session(&workspace, Vec::new()).await,
                        };
                        match session {
                            Ok(session) => {
                                previous_session = Some(session.id.clone());
                                attempt = 0;
                                let _ = events.send(UiEvent::AgentConnected { tab_id, session_id: session.id.clone() });
                                let mut updates = client.subscribe();
                                loop {
                                    tokio::select! {
                                        command = commands.recv() => match command {
                                            Some(AgentCommand::Prompt(blocks)) => { if let Err(error) = client.prompt(&session, blocks).await { let _ = events.send(UiEvent::AgentStatus { tab_id, status: error.to_string() }); } }
                                            Some(AgentCommand::Cancel) => { let _ = client.cancel(&session).await; }
                                            None => return,
                                        },
                                        event = updates.recv() => match event {
                                            Ok(AcpEvent::Disconnected) | Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                                            Ok(event) => { let _ = events.send(UiEvent::AgentEvent { tab_id, event }); }
                                            Err(tokio::sync::broadcast::error::RecvError::Lagged(count)) => { let _ = events.send(UiEvent::AgentStatus { tab_id, status: format!("ACP omitió {count} eventos") }); }
                                        },
                                    }
                                }
                            }
                            Err(error) => { let _ = events.send(UiEvent::AgentStatus { tab_id, status: format!("No se pudo abrir sesión ACP: {error}") }); }
                        }
                    }
                    let _ = process.wait().await;
                    attempt = attempt.saturating_add(1);
                    let delay = Duration::from_millis(250_u64.saturating_mul(1_u64 << attempt.min(5)));
                    let _ = events.send(UiEvent::AgentStatus { tab_id, status: format!("ACP desconectado; reintentando en {} ms", delay.as_millis()) });
                    tokio::time::sleep(delay).await;
                }
            });
        })
        .expect("spawn ACP worker");
}

fn discover_agent(cache: &std::path::Path) -> Option<AgentDefinition> {
    let mut registry = AgentRegistry::default();
    let _ = AgentRegistry::refresh_official_cache(cache);
    let _ = registry.import_official_cache(cache);
    registry.available().next().cloned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn coalesces_streaming_chunks() {
        let mut tab = AgentTab::new("Codex", PathBuf::from("."));
        tab.apply_update(
            &json!({"sessionUpdate":"agent_message_chunk","content":{"text":"hola "}}),
        );
        tab.apply_update(
            &json!({"sessionUpdate":"agent_message_chunk","content":{"text":"Forge"}}),
        );
        assert_eq!(
            tab.timeline,
            [TimelineItem::Message {
                role: MessageRole::Agent,
                text: "hola Forge".into()
            }]
        );
    }

    #[test]
    fn updates_tool_call_in_place() {
        let mut tab = AgentTab::new("Codex", PathBuf::from("."));
        tab.apply_update(&json!({"sessionUpdate":"tool_call","toolCallId":"1","title":"cargo test","status":"running"}));
        tab.apply_update(&json!({"sessionUpdate":"tool_call_update","toolCallId":"1","title":"cargo test","status":"completed"}));
        assert!(matches!(
            tab.timeline.as_slice(),
            [TimelineItem::ToolCall {
                state: ToolState::Succeeded,
                ..
            }]
        ));
    }

    #[test]
    fn scrolling_away_from_tail_survives_new_updates() {
        let mut tab = AgentTab::new("Codex", PathBuf::from("."));
        tab.visible_items = 2;
        for index in 0..5 {
            tab.apply_update(&json!({"type":"message_chunk","text":format!("{index}")}));
            tab.timeline.push(TimelineItem::Message {
                role: MessageRole::System,
                text: index.to_string(),
            });
        }
        tab.scroll_to_end();
        tab.scroll_by(-2);
        let before = tab.scroll_item;
        tab.apply_update(&json!({"type":"plan","entries":[]}));
        assert_eq!(tab.scroll_item, before);
    }

    #[test]
    fn file_mentions_are_confined_to_the_workspace() {
        let root = std::env::temp_dir().join(format!("forge-agent-context-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("notes.txt"), "contexto").unwrap();
        let mut tab = AgentTab::new("Codex", root.clone());
        tab.prompt = "revisa @notes.txt y @../fuera.txt".into();
        tab.submit_prompt();
        assert_eq!(tab.context.len(), 1);
        assert_eq!(tab.context[0].content, "contexto");
        std::fs::remove_dir_all(root).unwrap();
    }
}
