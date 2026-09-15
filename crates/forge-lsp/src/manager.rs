use crate::client::LspError;
use crate::diagnostics::DiagnosticStore;
use crate::process::{ServerInstance, ServerStatus};
use crate::registry::{LanguageRegistry, ServerConfig};
use crate::sync::path_to_uri;
use forge_buffer::Edit;
use lsp_types::*;
use ropey::Rope;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tracing::{error, info, warn};

/// Key identifying a unique server instance: `(server_name, workspace_root)`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ServerKey {
    pub server_name: String,
    pub root_path: PathBuf,
}

pub struct LspManager {
    registry: LanguageRegistry,
    servers: Arc<RwLock<HashMap<ServerKey, ServerInstance>>>,
    diagnostics: DiagnosticStore,
    debounce_delay: Duration,
    idle_timeout: Duration,
}

impl Default for LspManager {
    fn default() -> Self {
        Self::new(LanguageRegistry::new(), DiagnosticStore::new())
    }
}

impl LspManager {
    #[must_use]
    pub fn new(registry: LanguageRegistry, diagnostics: DiagnosticStore) -> Self {
        Self {
            registry,
            servers: Arc::new(RwLock::new(HashMap::new())),
            diagnostics,
            debounce_delay: Duration::from_millis(500),
            idle_timeout: Duration::from_secs(30 * 60), // 30 minutes
        }
    }

    #[must_use]
    pub fn diagnostics(&self) -> &DiagnosticStore {
        &self.diagnostics
    }

    #[must_use]
    pub fn registry(&self) -> &LanguageRegistry {
        &self.registry
    }

    pub fn set_debounce_delay(&mut self, delay: Duration) {
        self.debounce_delay = delay;
    }

    pub fn set_idle_timeout(&mut self, timeout: Duration) {
        self.idle_timeout = timeout;
    }

    /// Resolves the URI and server configs for a file.
    fn resolve_file_servers(&self, file_path: &Path) -> Option<(Uri, String, Vec<(ServerConfig, PathBuf)>)> {
        let lang = self.registry.detect_language(file_path)?;
        let uri = path_to_uri(file_path)?;

        let mut servers = Vec::new();
        for s_cfg in &lang.servers {
            let root = LanguageRegistry::find_workspace_root(file_path, &s_cfg.root_markers)
                .or_else(|| file_path.parent().map(Path::to_path_buf))?;
            servers.push((s_cfg.clone(), root));
        }

        Some((uri, lang.id.clone(), servers))
    }

    /// Opens a document: ensures required language server(s) are running and sends `didOpen`.
    pub async fn open_document(&self, file_path: &Path, text: String) -> Result<(), LspError> {
        let Some((uri, lang_id, server_targets)) = self.resolve_file_servers(file_path) else {
            return Ok(());
        };

        for (config, root_path) in server_targets {
            let key = ServerKey {
                server_name: config.name.clone(),
                root_path: root_path.clone(),
            };

            let mut servers = self.servers.write().await;
            if !servers.contains_key(&key) {
                match ServerInstance::new(config, root_path, self.diagnostics.clone()) {
                    Ok(mut instance) => {
                        if let Err(err) = instance.start().await {
                            warn!("Failed to start LSP server '{}': {err}", key.server_name);
                        }
                        servers.insert(key.clone(), instance);
                    }
                    Err(e) => {
                        error!("Failed to create LSP server instance: {e}");
                        continue;
                    }
                }
            }

            if let Some(instance) = servers.get_mut(&key) {
                instance.record_activity();
                if instance.status() == ServerStatus::Running {
                    let tracker_arc = instance.tracker();
                    let mut tracker = tracker_arc.lock().await;
                    let open_params = tracker.did_open(uri.clone(), lang_id.clone(), text.clone());
                    if let Some(client) = instance.client() {
                        let _ = client.did_open(open_params).await;
                    }
                }
            }
        }

        Ok(())
    }

    /// Sends incremental edits to all servers managing this document.
    pub async fn change_document(
        &self,
        file_path: &Path,
        edits: &[Edit],
        old_rope: &Rope,
        new_text: String,
    ) -> Result<(), LspError> {
        let Some((uri, _, server_targets)) = self.resolve_file_servers(file_path) else {
            return Ok(());
        };

        let mut servers = self.servers.write().await;
        for (config, root_path) in server_targets {
            let key = ServerKey {
                server_name: config.name,
                root_path,
            };

            if let Some(instance) = servers.get_mut(&key) {
                instance.record_activity();
                if instance.status() == ServerStatus::Running {
                    let tracker_arc = instance.tracker();
                    let mut tracker = tracker_arc.lock().await;
                    if let Some(change_params) = tracker.did_change_incremental(
                        &uri,
                        edits,
                        old_rope,
                        new_text.clone(),
                    ) {
                        if let Some(client) = instance.client() {
                            let _ = client.did_change(change_params).await;
                        }
                    }
                }
            }
        }

        Ok(())
    }

    /// Sends `didSave` to all servers managing this document.
    pub async fn save_document(&self, file_path: &Path, text: Option<String>) -> Result<(), LspError> {
        let Some((uri, _, server_targets)) = self.resolve_file_servers(file_path) else {
            return Ok(());
        };

        let mut servers = self.servers.write().await;
        for (config, root_path) in server_targets {
            let key = ServerKey {
                server_name: config.name,
                root_path,
            };

            if let Some(instance) = servers.get_mut(&key) {
                instance.record_activity();
                if instance.status() == ServerStatus::Running {
                    let tracker_arc = instance.tracker();
                    let tracker = tracker_arc.lock().await;
                    if let Some(save_params) = tracker.did_save(&uri, text.is_some()) {
                        if let Some(client) = instance.client() {
                            let _ = client.did_save(save_params).await;
                        }
                    }
                }
            }
        }

        Ok(())
    }

    /// Sends `didClose` to all servers managing this document.
    pub async fn close_document(&self, file_path: &Path) -> Result<(), LspError> {
        let Some((uri, _, server_targets)) = self.resolve_file_servers(file_path) else {
            return Ok(());
        };

        let mut servers = self.servers.write().await;
        for (config, root_path) in server_targets {
            let key = ServerKey {
                server_name: config.name,
                root_path,
            };

            if let Some(instance) = servers.get_mut(&key) {
                instance.record_activity();
                if instance.status() == ServerStatus::Running {
                    let tracker_arc = instance.tracker();
                    let mut tracker = tracker_arc.lock().await;
                    if let Some(close_params) = tracker.did_close(&uri) {
                        if let Some(client) = instance.client() {
                            let _ = client.did_close(close_params).await;
                        }
                    }
                }
            }
        }

        Ok(())
    }

    /// Requests code completion at a position in a file, merging completions across servers.
    pub async fn completion(
        &self,
        file_path: &Path,
        position: Position,
        trigger_char: Option<char>,
    ) -> Result<Vec<CompletionItem>, LspError> {
        let Some((uri, _, server_targets)) = self.resolve_file_servers(file_path) else {
            return Ok(Vec::new());
        };

        let params = CompletionParams {
            text_document_position: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
                position,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
            context: Some(CompletionContext {
                trigger_kind: if trigger_char.is_some() {
                    CompletionTriggerKind::TRIGGER_CHARACTER
                } else {
                    CompletionTriggerKind::INVOKED
                },
                trigger_character: trigger_char.map(|c| c.to_string()),
            }),
        };

        let servers = self.servers.read().await;
        let mut all_items = Vec::new();

        for (config, root_path) in server_targets {
            let key = ServerKey {
                server_name: config.name,
                root_path,
            };

            if let Some(instance) = servers.get(&key) {
                if instance.status() == ServerStatus::Running {
                    if let Some(client) = instance.client() {
                        if let Ok(Some(resp)) = client.completion(params.clone()).await {
                            match resp {
                                CompletionResponse::Array(items) => all_items.extend(items),
                                CompletionResponse::List(list) => all_items.extend(list.items),
                            }
                        }
                    }
                }
            }
        }

        // Deduplicate items by label
        let mut seen = std::collections::HashSet::new();
        all_items.retain(|item| seen.insert(item.label.clone()));

        // Sort items by sort_text or label
        all_items.sort_by(|a, b| {
            let a_key = a.sort_text.as_deref().unwrap_or(&a.label);
            let b_key = b.sort_text.as_deref().unwrap_or(&b.label);
            a_key.cmp(b_key)
        });

        Ok(all_items)
    }

    /// Requests hover information at a position, stacking hover contents if multiple servers respond.
    pub async fn hover(
        &self,
        file_path: &Path,
        position: Position,
    ) -> Result<Option<Hover>, LspError> {
        let Some((uri, _, server_targets)) = self.resolve_file_servers(file_path) else {
            return Ok(None);
        };

        let params = HoverParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
                position,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
        };

        let servers = self.servers.read().await;
        let mut contents = Vec::new();
        let mut range = None;

        for (config, root_path) in server_targets {
            let key = ServerKey {
                server_name: config.name,
                root_path,
            };

            if let Some(instance) = servers.get(&key) {
                if instance.status() == ServerStatus::Running {
                    if let Some(client) = instance.client() {
                        if let Ok(Some(hover)) = client.hover(params.clone()).await {
                            if range.is_none() {
                                range = hover.range;
                            }
                            match hover.contents {
                                HoverContents::Scalar(marked) => contents.push(marked_string_to_string(marked)),
                                HoverContents::Array(arr) => {
                                    for item in arr {
                                        contents.push(marked_string_to_string(item));
                                    }
                                }
                                HoverContents::Markup(markup) => contents.push(markup.value),
                            }
                        }
                    }
                }
            }
        }

        if contents.is_empty() {
            Ok(None)
        } else {
            Ok(Some(Hover {
                contents: HoverContents::Markup(MarkupContent {
                    kind: MarkupKind::Markdown,
                    value: contents.join("\n\n---\n\n"),
                }),
                range,
            }))
        }
    }

    /// Requests goto definition for a position in a file.
    pub async fn goto_definition(
        &self,
        file_path: &Path,
        position: Position,
    ) -> Result<Option<GotoDefinitionResponse>, LspError> {
        let Some((uri, _, server_targets)) = self.resolve_file_servers(file_path) else {
            return Ok(None);
        };

        let params = GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
                position,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };

        let servers = self.servers.read().await;
        for (config, root_path) in server_targets {
            let key = ServerKey {
                server_name: config.name,
                root_path,
            };

            if let Some(instance) = servers.get(&key) {
                if instance.status() == ServerStatus::Running {
                    if let Some(client) = instance.client() {
                        if let Ok(Some(res)) = client.goto_definition(params.clone()).await {
                            return Ok(Some(res));
                        }
                    }
                }
            }
        }

        Ok(None)
    }

    /// Requests references for a position in a file.
    pub async fn references(
        &self,
        file_path: &Path,
        position: Position,
        include_declaration: bool,
    ) -> Result<Vec<Location>, LspError> {
        let Some((uri, _, server_targets)) = self.resolve_file_servers(file_path) else {
            return Ok(Vec::new());
        };

        let params = ReferenceParams {
            text_document_position: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
                position,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
            context: ReferenceContext {
                include_declaration,
            },
        };

        let servers = self.servers.read().await;
        let mut results = Vec::new();

        for (config, root_path) in server_targets {
            let key = ServerKey {
                server_name: config.name,
                root_path,
            };

            if let Some(instance) = servers.get(&key) {
                if instance.status() == ServerStatus::Running {
                    if let Some(client) = instance.client() {
                        if let Ok(Some(locs)) = client.references(params.clone()).await {
                            results.extend(locs);
                        }
                    }
                }
            }
        }

        Ok(results)
    }

    /// Formats the whole document.
    pub async fn formatting(&self, file_path: &Path) -> Result<Option<Vec<TextEdit>>, LspError> {
        let Some((uri, _, server_targets)) = self.resolve_file_servers(file_path) else {
            return Ok(None);
        };

        let params = DocumentFormattingParams {
            text_document: TextDocumentIdentifier { uri },
            options: FormattingOptions {
                tab_size: 4,
                insert_spaces: true,
                ..Default::default()
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
        };

        let servers = self.servers.read().await;
        for (config, root_path) in server_targets {
            let key = ServerKey {
                server_name: config.name,
                root_path,
            };

            if let Some(instance) = servers.get(&key) {
                if instance.status() == ServerStatus::Running {
                    if let Some(client) = instance.client() {
                        if let Ok(Some(edits)) = client.formatting(params.clone()).await {
                            return Ok(Some(edits));
                        }
                    }
                }
            }
        }

        Ok(None)
    }

    /// Formats a range in the document.
    pub async fn range_formatting(
        &self,
        file_path: &Path,
        range: lsp_types::Range,
    ) -> Result<Option<Vec<TextEdit>>, LspError> {
        let Some((uri, _, server_targets)) = self.resolve_file_servers(file_path) else {
            return Ok(None);
        };

        let params = DocumentRangeFormattingParams {
            text_document: TextDocumentIdentifier { uri },
            range,
            options: FormattingOptions {
                tab_size: 4,
                insert_spaces: true,
                ..Default::default()
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
        };

        let servers = self.servers.read().await;
        for (config, root_path) in server_targets {
            let key = ServerKey {
                server_name: config.name,
                root_path,
            };

            if let Some(instance) = servers.get(&key) {
                if instance.status() == ServerStatus::Running {
                    if let Some(client) = instance.client() {
                        if let Ok(Some(edits)) = client.range_formatting(params.clone()).await {
                            return Ok(Some(edits));
                        }
                    }
                }
            }
        }

        Ok(None)
    }

    /// Requests code actions for a range in the document.
    pub async fn code_actions(
        &self,
        file_path: &Path,
        range: lsp_types::Range,
        diagnostics: Vec<lsp_types::Diagnostic>,
    ) -> Result<Vec<CodeActionOrCommand>, LspError> {
        let Some((uri, _, server_targets)) = self.resolve_file_servers(file_path) else {
            return Ok(Vec::new());
        };

        let params = CodeActionParams {
            text_document: TextDocumentIdentifier { uri },
            range,
            context: CodeActionContext {
                diagnostics,
                only: None,
                trigger_kind: Some(CodeActionTriggerKind::INVOKED),
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };

        let servers = self.servers.read().await;
        let mut results = Vec::new();

        for (config, root_path) in server_targets {
            let key = ServerKey {
                server_name: config.name,
                root_path,
            };

            if let Some(instance) = servers.get(&key) {
                if instance.status() == ServerStatus::Running {
                    if let Some(client) = instance.client() {
                        if let Ok(Some(res)) = client.code_action(params.clone()).await {
                            results.extend(res);
                        }
                    }
                }
            }
        }

        Ok(results)
    }

    /// Requests symbols in the document.
    pub async fn document_symbols(
        &self,
        file_path: &Path,
    ) -> Result<Option<DocumentSymbolResponse>, LspError> {
        let Some((uri, _, server_targets)) = self.resolve_file_servers(file_path) else {
            return Ok(None);
        };

        let params = DocumentSymbolParams {
            text_document: TextDocumentIdentifier { uri },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };

        let servers = self.servers.read().await;
        for (config, root_path) in server_targets {
            let key = ServerKey {
                server_name: config.name,
                root_path,
            };

            if let Some(instance) = servers.get(&key) {
                if instance.status() == ServerStatus::Running {
                    if let Some(client) = instance.client() {
                        if let Ok(Some(res)) = client.document_symbol(params.clone()).await {
                            return Ok(Some(res));
                        }
                    }
                }
            }
        }

        Ok(None)
    }

    /// Requests workspace symbols across all running language servers.
    pub async fn workspace_symbols(&self, query: &str) -> Result<Vec<SymbolInformation>, LspError> {
        let servers = self.servers.read().await;
        let mut results = Vec::new();

        for instance in servers.values() {
            if instance.status() == ServerStatus::Running {
                if let Some(client) = instance.client() {
                    if let Ok(Some(symbols)) = client.workspace_symbols(query).await {
                        results.extend(symbols);
                    }
                }
            }
        }

        Ok(results)
    }

    /// Periodic maintenance: shuts down idle servers and restarts crashed ones.
    pub async fn maintenance(&self) {
        let mut servers = self.servers.write().await;
        for (key, instance) in servers.iter_mut() {
            if instance.is_idle(self.idle_timeout) {
                info!("Shutting down idle LSP server '{}'", key.server_name);
                let _ = instance.stop().await;
            } else if instance.status() == ServerStatus::Crashed {
                let _ = instance.check_and_recover().await;
            }
        }
    }

    /// Shuts down all language servers cleanly.
    pub async fn shutdown_all(&self) {
        let mut servers = self.servers.write().await;
        for (_, instance) in servers.iter_mut() {
            let _ = instance.stop().await;
        }
    }
}

fn marked_string_to_string(marked: MarkedString) -> String {
    match marked {
        MarkedString::String(s) => s,
        MarkedString::LanguageString(ls) => format!("```{}\n{}\n```", ls.language, ls.value),
    }
}
