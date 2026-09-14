//! Agent-session state rendered by the GUI. Protocol I/O stays in
//! `proto-acp`; this module is deliberately pure so streaming updates can be
//! batched and tested without a GPUI window.

use forge_gui::i18n::{tr, trf};
use gpui::ScrollHandle;
use serde_json::Value;
use std::{
    future::Future,
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
                Some(method)
                    if method.starts_with("fs/")
                        || method.starts_with("terminal/")
                        || method == "session/request_permission" =>
                {
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
    Thought {
        id: String,
        text: String,
        active: bool,
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

pub use forge_buffer::ProposedEdit;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingPermissionOption {
    pub id: String,
    pub name: String,
    pub kind: String,
}

pub struct PendingPermissionRequest {
    pub id: serde_json::Value,
    pub tool_name: String,
    pub title: String,
    pub detail: String,
    pub capability: proto_acp::PermissionCapability,
    pub scope: String,
    pub options: Vec<PendingPermissionOption>,
    pub raw_options: Vec<serde_json::Value>,
    pub response: Option<tokio::sync::oneshot::Sender<proto_acp::JsonRpcMessage>>,
}

pub struct AgentTab {
    pub agent_name: String,
    pub session_id: Option<String>,
    pub status: String,
    pub prompt: String,
    /// Provider name this session was started with, if the router or the
    /// agent config chose one; shown as the route.
    pub provider: Option<String>,
    /// Human description of the route (`normal → openai: gpt-5`).
    pub route: String,
    /// Last prompt sent, for `agent.forward`.
    pub last_prompt: Option<String>,
    pub timeline: Vec<TimelineItem>,
    pub context: Vec<PromptContext>,
    /// Slash commands advertised by the ACP agent. Kept as session metadata;
    /// the protocol update must not become a raw JSON chat message.
    pub available_commands: Vec<(String, String)>,
    pub proposed_edits: Vec<ProposedEdit>,
    pub pending_permissions: Vec<PendingPermissionRequest>,
    turn_active: bool,
    pub scroll_handle: ScrollHandle,
    selection_anchor: Option<usize>,
    selection_head: Option<usize>,
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
            status: tr("Ready to start a session").into(),
            agent_name,
            session_id: None,
            prompt: String::new(),
            provider: None,
            route: String::new(),
            last_prompt: None,
            timeline: Vec::new(),
            context: Vec::new(),
            available_commands: Vec::new(),
            proposed_edits: Vec::new(),
            pending_permissions: Vec::new(),
            turn_active: false,
            scroll_handle: ScrollHandle::new(),
            selection_anchor: None,
            selection_head: None,
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

    #[must_use]
    pub fn turn_active(&self) -> bool {
        self.turn_active
    }

    pub fn finish_turn(&mut self, error: Option<String>) {
        self.turn_active = false;
        self.finish_thoughts();
        self.status = error.unwrap_or_else(|| tr("Ready").into());
    }

    pub fn resolve_permission(
        &mut self,
        request_id: &serde_json::Value,
        decision: proto_acp::PermissionDecision,
        ttl: proto_acp::PermissionTtl,
    ) -> Option<(proto_acp::PermissionCapability, String)> {
        let index = self
            .pending_permissions
            .iter()
            .position(|p| &p.id == request_id)?;
        let mut pending = self.pending_permissions.remove(index);
        let option_id = proto_acp::select_option_id(&pending.raw_options, decision, ttl)
            .or_else(|| pending.options.first().map(|opt| opt.id.clone()));
        if let Some(sender) = pending.response.take() {
            let reply = proto_acp::JsonRpcMessage::response(
                pending.id.clone(),
                serde_json::json!({
                    "outcome": {
                        "outcome": "selected",
                        "optionId": option_id,
                    }
                }),
            );
            let _ = sender.send(reply);
        }
        let perm_id = format!("perm-{}", pending.id);
        if let Some(TimelineItem::ToolCall { state, detail, .. }) = self
            .timeline
            .iter_mut()
            .find(|item| matches!(item, TimelineItem::ToolCall { id, .. } if id == &perm_id))
        {
            match decision {
                proto_acp::PermissionDecision::Allow => {
                    *state = ToolState::Running;
                    *detail = trf("Permission granted ({})", &[&format!("{ttl:?}")]);
                }
                proto_acp::PermissionDecision::Deny => {
                    *state = ToolState::Failed;
                    *detail = tr("Permission denied by the user").into();
                }
                proto_acp::PermissionDecision::Ask => {}
            }
        }
        Some((pending.capability, pending.scope))
    }

    pub fn submit_prompt(&mut self) -> Option<String> {
        if self.turn_active {
            return None;
        }
        let prompt = self.prompt.trim().to_owned();
        if prompt.is_empty() {
            return None;
        }
        self.prompt.clear();
        self.resolve_file_mentions(&prompt);
        self.last_prompt = Some(prompt.clone());
        self.turn_active = true;
        self.timeline.push(TimelineItem::Message {
            role: MessageRole::User,
            text: prompt.clone(),
        });
        self.status = tr("Waiting for the agent…").into();
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
        self.scroll_handle.scroll_to_top_of_item(self.scroll_item);
    }

    pub fn scroll_to_end(&mut self) {
        self.scroll_item = self.timeline.len().saturating_sub(self.visible_items);
        self.following_tail = true;
        self.scroll_handle.scroll_to_bottom();
    }

    pub fn stop_following_tail(&mut self) {
        self.following_tail = false;
    }

    pub fn start_selection(&mut self, index: usize) {
        self.selection_anchor = Some(index);
        self.selection_head = Some(index);
    }

    pub fn extend_selection(&mut self, index: usize) {
        if self.selection_anchor.is_some() {
            self.selection_head = Some(index);
        }
    }

    #[must_use]
    pub fn item_selected(&self, index: usize) -> bool {
        let (Some(anchor), Some(head)) = (self.selection_anchor, self.selection_head) else {
            return false;
        };
        let range = anchor.min(head)..=anchor.max(head);
        range.contains(&index)
    }

    #[must_use]
    pub fn selected_text(&self) -> Option<String> {
        let (Some(anchor), Some(head)) = (self.selection_anchor, self.selection_head) else {
            return None;
        };
        let start = anchor.min(head);
        let end = anchor.max(head).min(self.timeline.len().saturating_sub(1));
        let text = self
            .timeline
            .get(start..=end)?
            .iter()
            .map(timeline_text)
            .collect::<Vec<_>>()
            .join("\n\n");
        (!text.is_empty()).then_some(text)
    }

    /// Applies one tolerant `session/update`. Unknown update shapes are logged
    /// instead of leaking protocol JSON into the conversation timeline.
    pub fn apply_update(&mut self, update: &Value) {
        let follow_tail = self.following_tail;
        let kind = update
            .get("sessionUpdate")
            .or_else(|| update.get("type"))
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        match kind {
            "available_commands_update" => {
                self.available_commands = update
                    .get("availableCommands")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|command| {
                        Some((
                            command.get("name")?.as_str()?.to_owned(),
                            command
                                .get("description")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_owned(),
                        ))
                    })
                    .collect();
            }
            "agent_message_chunk" | "message_chunk" => {
                self.finish_thoughts();
                let text = update
                    .get("content")
                    .and_then(|content| content.get("text"))
                    .or_else(|| update.get("text"))
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                self.append_agent_chunk(text);
                self.status = tr("Receiving the answer…").into();
            }
            "agent_thought_chunk" | "thought_chunk" => {
                let id = update
                    .get("messageId")
                    .and_then(Value::as_str)
                    .unwrap_or("thought");
                let text = update
                    .get("content")
                    .and_then(|content| content.get("text"))
                    .or_else(|| update.get("text"))
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                self.append_thought_chunk(id, text);
                self.status = tr("Thinking…").into();
            }
            "tool_call" | "tool_call_update" => {
                self.finish_thoughts();
                self.apply_tool_update(update);
            }
            "plan" => {
                self.finish_thoughts();
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
            "current_mode_update" | "config_option_update" | "config_options_update" => {}
            _ => tracing::debug!(kind, update = %update, "unhandled ACP session update"),
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

    fn append_thought_chunk(&mut self, id: &str, chunk: &str) {
        if chunk.is_empty() {
            return;
        }
        if let Some(TimelineItem::Thought { text, active, .. }) =
            self.timeline.iter_mut().rev().find(
                |item| matches!(item, TimelineItem::Thought { id: old_id, .. } if old_id == id),
            )
        {
            text.push_str(chunk);
            *active = true;
        } else {
            self.timeline.push(TimelineItem::Thought {
                id: id.to_owned(),
                text: chunk.to_owned(),
                active: true,
            });
        }
    }

    fn finish_thoughts(&mut self) {
        for item in &mut self.timeline {
            if let TimelineItem::Thought { active, .. } = item {
                *active = false;
            }
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
}

fn timeline_text(item: &TimelineItem) -> String {
    match item {
        TimelineItem::Message { text, .. } | TimelineItem::Thought { text, .. } => text.clone(),
        TimelineItem::ToolCall { title, detail, .. } => format!("{title}\n{detail}"),
        TimelineItem::Plan { title, entries } => format!("{title}\n{}", entries.join("\n")),
    }
}

pub fn spawn_agent_worker(
    definition: Option<AgentDefinition>,
    registry_cache: PathBuf,
    workspace: PathBuf,
    mcp_servers: Vec<serde_json::Value>,
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
                    status: tr("No ACP adapter found in PATH").into(),
                });
                return;
            };
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build();
            let Ok(runtime) = runtime else {
                let _ = events.send(UiEvent::AgentStatus { tab_id, status: tr("Could not create the ACP runtime").into() });
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
                            let _ = events.send(UiEvent::AgentStatus { tab_id, status: trf("ACP could not start: {}", &[&error]) });
                            return;
                        }
                    };
                    let client = process.client.clone();
                    let initialized = client.initialize("Forge", env!("CARGO_PKG_VERSION")).await;
                    if let Err(error) = initialized {
                        let _ = events.send(UiEvent::AgentStatus { tab_id, status: trf("ACP initialize failed: {}", &[&error]) });
                    } else {
                        if let Some(method) = &definition.auth_method { let _ = client.authenticate(method).await; }
                        let session = match previous_session.as_deref() {
                            Some(id) => match client.load_session(id, &workspace, mcp_servers.clone()).await {
                                Ok(session) => Ok(session),
                                Err(_) => client.new_session(&workspace, mcp_servers.clone()).await,
                            },
                            None => client.new_session(&workspace, mcp_servers.clone()).await,
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
                                            Some(AgentCommand::Prompt(blocks)) => {
                                                // `session/prompt` stays pending for the whole turn.
                                                // Run it separately so this loop can forward every
                                                // streaming `session/update` as soon as it arrives.
                                                let prompt_client = client.clone();
                                                let prompt_session = session.clone();
                                                let prompt_events = events.clone();
                                                tokio::spawn(async move {
                                                    let error = prompt_client
                                                        .prompt(&prompt_session, blocks)
                                                        .await
                                                        .err()
                                                        .map(|error| error.to_string());
                                                    let _ = prompt_events.send(UiEvent::AgentTurnFinished {
                                                        tab_id,
                                                        error,
                                                    });
                                                });
                                            }
                                            Some(AgentCommand::Cancel) => { let _ = client.cancel(&session).await; }
                                            None => return,
                                        },
                                        event = updates.recv() => match event {
                                            Ok(AcpEvent::Disconnected) | Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                                            Ok(event) => { let _ = events.send(UiEvent::AgentEvent { tab_id, event }); }
                                            Err(tokio::sync::broadcast::error::RecvError::Lagged(count)) => { let _ = events.send(UiEvent::AgentStatus { tab_id, status: trf("ACP skipped {} events", &[&count]) }); }
                                        },
                                    }
                                }
                            }
                            Err(error) => { let _ = events.send(UiEvent::AgentStatus { tab_id, status: trf("Could not open the ACP session: {}", &[&error]) }); }
                        }
                    }
                    let _ = process.wait().await;
                    attempt = attempt.saturating_add(1);
                    let delay = Duration::from_millis(250_u64.saturating_mul(1_u64 << attempt.min(5)));
                    let _ = events.send(UiEvent::AgentStatus { tab_id, status: trf("ACP disconnected; retrying in {} ms", &[&delay.as_millis()]) });
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
    fn available_commands_are_metadata_not_raw_timeline_messages() {
        let mut tab = AgentTab::new("OpenCode", PathBuf::from("."));
        tab.apply_update(&json!({
            "sessionUpdate": "available_commands_update",
            "availableCommands": [{"name": "review", "description": "Review changes"}]
        }));
        assert_eq!(
            tab.available_commands,
            [("review".to_owned(), "Review changes".to_owned())]
        );
        assert!(tab.timeline.is_empty());
    }

    #[test]
    fn thought_chunks_stream_into_one_live_timeline_card() {
        let mut tab = AgentTab::new("OpenCode", PathBuf::from("."));
        tab.apply_update(&json!({
            "sessionUpdate": "agent_thought_chunk", "messageId": "m1",
            "content": {"type": "text", "text": "Voy a "}
        }));
        tab.apply_update(&json!({
            "sessionUpdate": "agent_thought_chunk", "messageId": "m1",
            "content": {"type": "text", "text": "revisarlo"}
        }));
        assert!(matches!(
            tab.timeline.as_slice(),
            [TimelineItem::Thought { id, text, active: true }]
                if id == "m1" && text == "Voy a revisarlo"
        ));
        tab.apply_update(&json!({
            "sessionUpdate": "agent_message_chunk",
            "content": {"type": "text", "text": "Resultado"}
        }));
        assert!(matches!(
            tab.timeline[0],
            TimelineItem::Thought { active: false, .. }
        ));
        assert_eq!(tab.timeline.len(), 2);
    }

    #[test]
    fn one_turn_at_a_time_tracks_live_agent_activity() {
        let mut tab = AgentTab::new("OpenCode", PathBuf::from("."));
        tab.prompt = "primero".into();
        assert_eq!(tab.submit_prompt().as_deref(), Some("primero"));
        assert!(tab.turn_active());

        tab.prompt = "segundo".into();
        assert_eq!(tab.submit_prompt(), None);
        assert_eq!(tab.prompt, "segundo");

        tab.finish_turn(None);
        assert!(!tab.turn_active());
        assert_eq!(tab.status, tr("Ready"));
        assert_eq!(tab.submit_prompt().as_deref(), Some("segundo"));
    }

    #[test]
    fn selecting_timeline_items_produces_copyable_text() {
        let mut tab = AgentTab::new("OpenCode", PathBuf::from("."));
        tab.timeline.push(TimelineItem::Message {
            role: MessageRole::User,
            text: "pregunta".into(),
        });
        tab.timeline.push(TimelineItem::Message {
            role: MessageRole::Agent,
            text: "respuesta".into(),
        });

        tab.start_selection(1);
        tab.extend_selection(0);
        assert!(tab.item_selected(0));
        assert!(tab.item_selected(1));
        assert_eq!(
            tab.selected_text().as_deref(),
            Some("pregunta\n\nrespuesta")
        );
    }

    #[test]
    fn scrolling_away_from_tail_survives_new_updates() {
        let mut tab = AgentTab::new("Codex", PathBuf::from("."));
        tab.visible_items = 2;
        for index in 0..5 {
            tab.apply_update(&json!({"type":"message_chunk","text":format!("{index}")}));
            tab.timeline.push(TimelineItem::Message {
                role: MessageRole::Agent,
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

    #[test]
    fn resolves_permission_requests_with_correct_decision_and_ttl() {
        use tokio::sync::oneshot;
        let mut tab = AgentTab::new("Codex", PathBuf::from("."));
        let (tx, rx) = oneshot::channel();
        let request_id = json!("perm-test-1");
        tab.pending_permissions.push(PendingPermissionRequest {
            id: request_id.clone(),
            tool_name: "fs/write_text_file".into(),
            title: "Write file".into(),
            detail: "src/main.rs".into(),
            capability: proto_acp::PermissionCapability::FsWrite,
            scope: "src/main.rs".into(),
            options: vec![
                PendingPermissionOption {
                    id: "allow_once".into(),
                    name: "Allow once".into(),
                    kind: "allow_once".into(),
                },
                PendingPermissionOption {
                    id: "deny".into(),
                    name: "Deny".into(),
                    kind: "deny".into(),
                },
            ],
            raw_options: vec![
                json!({"id": "allow_once", "kind": "allow_once"}),
                json!({"id": "deny", "kind": "deny"}),
            ],
            response: Some(tx),
        });

        assert_eq!(tab.pending_permissions.len(), 1);
        let resolved = tab.resolve_permission(
            &request_id,
            proto_acp::PermissionDecision::Allow,
            proto_acp::PermissionTtl::Once,
        );
        assert_eq!(
            resolved,
            Some((
                proto_acp::PermissionCapability::FsWrite,
                "src/main.rs".to_owned()
            ))
        );
        assert!(tab.pending_permissions.is_empty());

        let response_msg = rx.blocking_recv().expect("response should be received");
        let outcome = response_msg.result.as_ref().expect("result");
        assert_eq!(outcome["outcome"]["outcome"], "selected");
        assert_eq!(outcome["outcome"]["optionId"], "allow_once");
    }

    #[test]
    fn proposed_edits_track_hunks_and_apply_to_buffer() {
        let mut buffer = forge_buffer::Buffer::new("line1\nline2\nline3\n");
        let proposed = forge_buffer::ProposedEdit::from_proposal(
            PathBuf::from("test.txt"),
            "Agent",
            "line1\nline2\nline3\n".into(),
            &buffer,
            "line1\nmodified line2\nline3\n".into(),
        );
        assert_eq!(proposed.hunks.len(), 1);
        assert_eq!(proposed.pending_count(), 1);
        assert!(!proposed.is_all_resolved());

        let mut tab = AgentTab::new("Codex", PathBuf::from("."));
        tab.proposed_edits.push(proposed);

        let edit = &mut tab.proposed_edits[0];
        let _tx = edit.apply_hunk(0, &mut buffer).expect("apply hunk");
        assert_eq!(buffer.text(), "line1\nmodified line2\nline3\n");
        assert_eq!(edit.pending_count(), 0);
        assert!(edit.is_all_resolved());
    }
}
