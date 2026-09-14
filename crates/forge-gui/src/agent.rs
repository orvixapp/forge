//! Agent-session state rendered by the GUI. Protocol I/O stays in
//! `proto-acp`; this module is deliberately pure so streaming updates can be
//! batched and tested without a GPUI window.

use serde_json::Value;
use std::{
    ops::Range,
    path::PathBuf,
    sync::{Arc, mpsc::Sender},
    time::Duration,
};
use tokio::sync::mpsc as async_mpsc;

use crate::ipc::UiEvent;
use proto_acp::{
    AcpEvent, AgentDefinition, AgentProcess, ClientSurface, PermissionChoice, PromptBlock,
};

pub enum AgentCommand {
    Prompt(String),
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

pub struct AgentTab {
    pub agent_name: String,
    pub session_id: Option<String>,
    pub status: String,
    pub prompt: String,
    pub timeline: Vec<TimelineItem>,
    commands: Option<async_mpsc::UnboundedSender<AgentCommand>>,
}

impl AgentTab {
    #[must_use]
    pub fn new(agent_name: impl Into<String>) -> Self {
        let agent_name = agent_name.into();
        Self {
            status: "Lista para iniciar sesión".into(),
            agent_name,
            session_id: None,
            prompt: String::new(),
            timeline: Vec::new(),
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
        self.timeline.push(TimelineItem::Message {
            role: MessageRole::User,
            text: prompt.clone(),
        });
        self.status = "Esperando al agente…".into();
        if let Some(commands) = &self.commands {
            let _ = commands.send(AgentCommand::Prompt(prompt.clone()));
        }
        Some(prompt)
    }

    /// Applies one tolerant `session/update`. Unknown update shapes are kept
    /// as system messages, making optional vendor capabilities visible rather
    /// than fatal to the session.
    pub fn apply_update(&mut self, update: &Value) {
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
    definition: AgentDefinition,
    workspace: PathBuf,
    tab_id: u64,
    events: Sender<UiEvent>,
    mut commands: async_mpsc::UnboundedReceiver<AgentCommand>,
) {
    std::thread::Builder::new()
        .name(format!("forge-agent-{tab_id}"))
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build();
            let Ok(runtime) = runtime else {
                let _ = events.send(UiEvent::AgentStatus { tab_id, status: "No se pudo crear el runtime ACP".into() });
                return;
            };
            runtime.block_on(async move {
                let surface = Arc::new(ClientSurface::new(workspace.clone(), PermissionChoice::Deny));
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
                                            Some(AgentCommand::Prompt(text)) => { if let Err(error) = client.prompt(&session, vec![PromptBlock::Text { text }]).await { let _ = events.send(UiEvent::AgentStatus { tab_id, status: error.to_string() }); } }
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn coalesces_streaming_chunks() {
        let mut tab = AgentTab::new("Codex");
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
        let mut tab = AgentTab::new("Codex");
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
}
