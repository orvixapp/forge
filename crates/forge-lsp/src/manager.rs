use crate::client::LspError;
use crate::diagnostics::DiagnosticStore;
use crate::process::{ServerInstance, ServerStatus};
use crate::registry::{LanguageRegistry, ServerConfig};
use crate::sync::path_to_uri;
use forge_buffer::Edit;
use lsp_types::{
    CodeAction, CodeActionContext, CodeActionOrCommand, CodeActionParams, CodeActionTriggerKind,
    CompletionContext, CompletionItem, CompletionParams, CompletionResponse, CompletionTriggerKind,
    DocumentDiagnosticParams, DocumentDiagnosticReport, DocumentDiagnosticReportResult,
    DocumentFormattingParams, DocumentRangeFormattingParams, DocumentSymbolParams,
    DocumentSymbolResponse, FormattingOptions, GotoDefinitionParams, GotoDefinitionResponse, Hover,
    HoverContents, HoverParams, Location, MarkedString, MarkupContent, MarkupKind,
    PartialResultParams, Position, PrepareRenameResponse, ReferenceContext, ReferenceParams,
    RenameParams, SymbolInformation, TextDocumentIdentifier, TextDocumentPositionParams, TextEdit,
    Uri, WorkDoneProgressParams, WorkspaceEdit,
};
use ropey::Rope;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tracing::{error, info};

/// Key identifying a unique server instance: `(server_name, workspace_root)`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ServerKey {
    pub server_name: String,
    pub root_path: PathBuf,
}

struct ServerView {
    status: ServerStatus,
    client: Option<crate::client::LspClient>,
    capabilities: serde_json::Value,
}
impl ServerView {
    fn status(&self) -> ServerStatus {
        self.status
    }
    fn client(&self) -> Option<&crate::client::LspClient> {
        self.client.as_ref()
    }
    fn supports(&self, key: &str) -> bool {
        self.capabilities
            .get(key)
            .is_some_and(|value| !value.is_null() && value != &serde_json::Value::Bool(false))
    }
    /// Whether `key.field` is announced as `true` (`completionProvider.resolveProvider`).
    fn flag(&self, key: &str, field: &str) -> bool {
        self.capabilities
            .get(key)
            .and_then(|value| value.get(field))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
    }
    fn triggers_completion(&self, character: char) -> bool {
        self.capabilities
            .get("completionProvider")
            .and_then(|value| value.get("triggerCharacters"))
            .and_then(serde_json::Value::as_array)
            .is_some_and(|characters| {
                characters
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .any(|trigger| trigger.chars().eq(std::iter::once(character)))
            })
    }
}

/// Indentation the server should format with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Indentation {
    pub tab_size: u32,
    pub insert_spaces: bool,
}

impl Default for Indentation {
    fn default() -> Self {
        Self {
            tab_size: 4,
            insert_spaces: true,
        }
    }
}

impl Indentation {
    fn options(self) -> FormattingOptions {
        FormattingOptions {
            tab_size: self.tab_size,
            insert_spaces: self.insert_spaces,
            trim_trailing_whitespace: Some(true),
            insert_final_newline: Some(true),
            ..Default::default()
        }
    }
}

fn pull_source(server_name: &str) -> String {
    format!("{server_name}/pull")
}

/// Source key of a server's `publishDiagnostics` in the [`DiagnosticStore`].
#[must_use]
pub fn push_source(server_name: &str) -> String {
    format!("{server_name}/push")
}

type FileServers = (Uri, String, Vec<(ServerConfig, PathBuf)>);
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
            idle_timeout: Duration::from_mins(30), // 30 minutes
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

    /// Forwards the editor's deduplicated file watcher events to matching roots.
    pub async fn watched_files(&self, changes: HashMap<PathBuf, bool>) {
        if changes.is_empty() {
            return;
        }
        for (key, server) in self.server_views().await {
            let events: Vec<_> = changes
                .iter()
                .filter(|(path, _)| path.starts_with(&key.root_path))
                .filter_map(|(path, removed)| {
                    Some(lsp_types::FileEvent {
                        uri: path_to_uri(path)?,
                        typ: if *removed {
                            lsp_types::FileChangeType::DELETED
                        } else {
                            lsp_types::FileChangeType::CHANGED
                        },
                    })
                })
                .collect();
            if !events.is_empty()
                && server.status == ServerStatus::Running
                && let Some(client) = server.client()
            {
                let _ = client
                    .send_notification(
                        "workspace/didChangeWatchedFiles",
                        serde_json::to_value(lsp_types::DidChangeWatchedFilesParams {
                            changes: events,
                        })
                        .ok(),
                    )
                    .await;
            }
        }
    }

    async fn server_views(&self) -> HashMap<ServerKey, ServerView> {
        self.servers
            .read()
            .await
            .iter()
            .map(|(key, instance)| {
                (
                    key.clone(),
                    ServerView {
                        status: instance.status(),
                        client: instance.client().cloned(),
                        capabilities: instance
                            .server_capabilities()
                            .and_then(|caps| serde_json::to_value(caps).ok())
                            .unwrap_or_default(),
                    },
                )
            })
            .collect()
    }

    /// Resolves the URI and server configs for a file.
    fn resolve_file_servers(&self, file_path: &Path) -> Option<FileServers> {
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
    ///
    /// # Errors
    /// Returns transport, serialization, protocol or lifecycle errors.
    pub async fn open_document(&self, file_path: &Path, text: String) -> Result<(), LspError> {
        self.open_document_versioned(file_path, text, 1).await
    }

    /// Opens the live editor snapshot at its real buffer revision.
    ///
    /// # Errors
    /// Returns transport, serialization, protocol or lifecycle errors.
    pub async fn open_document_versioned(
        &self,
        file_path: &Path,
        text: String,
        version: u64,
    ) -> Result<(), LspError> {
        let version = i32::try_from(version)
            .map_err(|_| LspError::Channel("Buffer revision exceeds LSP integer range".into()))?;
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
                        instance.start().await?;
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
                if matches!(
                    instance.status(),
                    ServerStatus::Stopped | ServerStatus::Crashed
                ) {
                    instance.start().await?;
                }
                if instance.status() == ServerStatus::Running {
                    let tracker_arc = instance.tracker();
                    let mut tracker = tracker_arc.lock().await;
                    if tracker.is_open(&uri) {
                        continue;
                    }
                    let open_params = tracker.did_open_versioned(
                        uri.clone(),
                        lang_id.clone(),
                        text.clone(),
                        version,
                    );
                    if let Some(client) = instance.client() {
                        let _ = client.did_open(open_params).await;
                    }
                }
            }
        }

        Ok(())
    }

    /// Sends incremental edits to all servers managing this document.
    ///
    /// # Errors
    /// Returns transport, serialization, protocol or lifecycle errors.
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
                    if let Some(change_params) =
                        tracker.did_change_incremental(&uri, edits, old_rope, new_text.clone())
                        && let Some(client) = instance.client()
                    {
                        let _ = client.did_change(change_params).await;
                    }
                }
            }
        }

        Ok(())
    }

    /// Synchronizes a live snapshot using a precise buffer revision.
    ///
    /// # Errors
    /// Returns transport, serialization, protocol or lifecycle errors.
    pub async fn change_document_versioned(
        &self,
        file_path: &Path,
        edits: &[Edit],
        old_rope: &Rope,
        new_text: String,
        version: u64,
    ) -> Result<(), LspError> {
        let version = i32::try_from(version)
            .map_err(|_| LspError::Channel("Buffer revision exceeds LSP integer range".into()))?;
        let Some((uri, _, targets)) = self.resolve_file_servers(file_path) else {
            return Ok(());
        };
        let mut servers = self.servers.write().await;
        for (config, root_path) in targets {
            let key = ServerKey {
                server_name: config.name,
                root_path,
            };
            if let Some(instance) = servers.get_mut(&key) {
                instance.record_activity();
                if instance.status() == ServerStatus::Stopped {
                    instance.start().await?;
                }
                let tracker = instance.tracker();
                let mut tracker = tracker.lock().await;
                let Some(doc) = tracker.get(&uri) else {
                    continue;
                };
                if doc.version >= version {
                    continue;
                }
                let sync = instance
                    .server_capabilities()
                    .and_then(|caps| caps.text_document_sync.as_ref());
                let kind = match sync {
                    Some(lsp_types::TextDocumentSyncCapability::Kind(kind)) => *kind,
                    Some(lsp_types::TextDocumentSyncCapability::Options(options)) => options
                        .change
                        .unwrap_or(lsp_types::TextDocumentSyncKind::NONE),
                    None => lsp_types::TextDocumentSyncKind::NONE,
                };
                let params = if kind == lsp_types::TextDocumentSyncKind::INCREMENTAL {
                    tracker.did_change_incremental(&uri, edits, old_rope, new_text.clone())
                } else {
                    tracker.did_change_full(&uri, new_text.clone())
                };
                if let Some(mut params) = params {
                    params.text_document.version = version;
                    tracker.set_version(&uri, version);
                    if kind != lsp_types::TextDocumentSyncKind::NONE
                        && let Some(client) = instance.client()
                    {
                        client.did_change(params).await?;
                    }
                }
            }
        }
        Ok(())
    }

    /// Sends `didSave` to all servers managing this document.
    ///
    /// # Errors
    /// Returns transport, serialization, protocol or lifecycle errors.
    pub async fn save_document(
        &self,
        file_path: &Path,
        text: Option<String>,
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
                    let tracker = tracker_arc.lock().await;
                    let save = instance
                        .server_capabilities()
                        .and_then(|caps| caps.text_document_sync.as_ref())
                        .and_then(|sync| match sync {
                            lsp_types::TextDocumentSyncCapability::Options(options) => {
                                options.save.as_ref()
                            }
                            lsp_types::TextDocumentSyncCapability::Kind(_) => None,
                        });
                    let Some(include_text) = save.and_then(|save| match save {
                        lsp_types::TextDocumentSyncSaveOptions::Supported(true) => Some(false),
                        lsp_types::TextDocumentSyncSaveOptions::SaveOptions(options) => {
                            Some(options.include_text.unwrap_or(false))
                        }
                        lsp_types::TextDocumentSyncSaveOptions::Supported(false) => None,
                    }) else {
                        continue;
                    };
                    if let Some(save_params) =
                        tracker.did_save(&uri, include_text && text.is_some())
                        && let Some(client) = instance.client()
                    {
                        let _ = client.did_save(save_params).await;
                    }
                }
            }
        }

        Ok(())
    }

    /// Sends `didClose` to all servers managing this document.
    ///
    /// # Errors
    /// Returns transport, serialization, protocol or lifecycle errors.
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
                let tracker_arc = instance.tracker();
                let mut tracker = tracker_arc.lock().await;
                if let Some(close_params) = tracker.did_close(&uri) {
                    // Pull reports only describe open documents; push ones
                    // are the server's to clear (cargo check keeps finding
                    // errors in files that are not open).
                    self.diagnostics
                        .clear_source(&uri, &pull_source(&key.server_name));
                    if instance.status() == ServerStatus::Running
                        && let Some(client) = instance.client()
                    {
                        let _ = client.did_close(close_params).await;
                    }
                }
            }
        }

        Ok(())
    }

    /// `textDocument/diagnostic` (LSP 3.17) for every server advertising a
    /// `diagnosticProvider`; the report is stored at `version` under the
    /// server's pull source so it merges with published diagnostics.
    /// Returns how many diagnostics the servers reported (an `unchanged`
    /// answer counts as one), so callers can retry while a server is
    /// still indexing and answers with nothing.
    ///
    /// # Errors
    /// Returns transport or protocol errors.
    pub async fn pull_diagnostics(
        &self,
        file_path: &Path,
        version: u64,
    ) -> Result<usize, LspError> {
        let Some((uri, _, targets)) = self.resolve_file_servers(file_path) else {
            return Ok(0);
        };
        let version = i32::try_from(version).ok();
        let servers = self.server_views().await;
        let mut reported = 0;
        for (config, root_path) in targets {
            let key = ServerKey {
                server_name: config.name,
                root_path,
            };
            let Some(view) = servers.get(&key) else {
                continue;
            };
            if view.status() != ServerStatus::Running || !view.supports("diagnosticProvider") {
                continue;
            }
            let Some(client) = view.client() else {
                continue;
            };
            let tracker = {
                let servers = self.servers.read().await;
                servers.get(&key).map(ServerInstance::tracker)
            };
            let Some(tracker) = tracker else { continue };
            let previous_result_id = tracker
                .lock()
                .await
                .get(&uri)
                .and_then(|doc| doc.pull_result_id.clone());
            let params = DocumentDiagnosticParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                identifier: None,
                previous_result_id,
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
            };
            let report: DocumentDiagnosticReportResult = client
                .send_request("textDocument/diagnostic", params)
                .await?;
            let source = pull_source(&key.server_name);
            match report {
                DocumentDiagnosticReportResult::Report(DocumentDiagnosticReport::Full(full)) => {
                    tracker
                        .lock()
                        .await
                        .set_pull_result_id(&uri, full.full_document_diagnostic_report.result_id);
                    reported += full.full_document_diagnostic_report.items.len();
                    self.diagnostics.update_from(
                        &uri,
                        &source,
                        version,
                        full.full_document_diagnostic_report.items,
                    );
                    for (related, report) in full.related_documents.into_iter().flatten() {
                        if let lsp_types::DocumentDiagnosticReportKind::Full(report) = report {
                            reported += report.items.len();
                            self.diagnostics
                                .update_from(&related, &source, None, report.items);
                        }
                    }
                }
                DocumentDiagnosticReportResult::Report(DocumentDiagnosticReport::Unchanged(_)) => {
                    reported += 1;
                }
                DocumentDiagnosticReportResult::Partial(_) => {}
            }
        }
        Ok(reported)
    }

    /// Requests code completion at a position in a file, merging completions across servers.
    ///
    /// # Errors
    /// Returns transport, serialization, protocol or lifecycle errors.
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

        let servers = self.server_views().await;
        let mut all_items = Vec::new();

        for (config, root_path) in server_targets {
            let key = ServerKey {
                server_name: config.name,
                root_path,
            };

            if let Some(instance) = servers.get(&key)
                && instance.status() == ServerStatus::Running
                && instance.supports("completionProvider")
                && trigger_char.is_none_or(|character| instance.triggers_completion(character))
                && let Some(client) = instance.client()
                && let Ok(Some(resp)) = client.completion(params.clone()).await
            {
                match resp {
                    CompletionResponse::Array(items) => all_items.extend(items),
                    CompletionResponse::List(list) => all_items.extend(list.items),
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

    /// Whether any server of this file completes after `character`.
    #[must_use]
    pub async fn completion_trigger(&self, file_path: &Path, character: char) -> bool {
        let Some((_, _, targets)) = self.resolve_file_servers(file_path) else {
            return false;
        };
        let servers = self.server_views().await;
        targets.into_iter().any(|(config, root_path)| {
            servers
                .get(&ServerKey {
                    server_name: config.name,
                    root_path,
                })
                .is_some_and(|view| view.triggers_completion(character))
        })
    }

    /// `completionItem/resolve` when the server offers it; otherwise the
    /// item is returned as is.
    ///
    /// # Errors
    /// Returns transport or protocol errors.
    pub async fn resolve_completion(
        &self,
        file_path: &Path,
        item: CompletionItem,
    ) -> Result<CompletionItem, LspError> {
        let Some((_, _, targets)) = self.resolve_file_servers(file_path) else {
            return Ok(item);
        };
        let servers = self.server_views().await;
        for (config, root_path) in targets {
            if let Some(view) = servers.get(&ServerKey {
                server_name: config.name,
                root_path,
            }) && view.status() == ServerStatus::Running
                && view.flag("completionProvider", "resolveProvider")
                && let Some(client) = view.client()
            {
                return client.resolve_completion_item(item).await;
            }
        }
        Ok(item)
    }

    /// Requests goto implementation for a position in a file.
    ///
    /// # Errors
    /// Returns transport, serialization, protocol or lifecycle errors.
    pub async fn goto_implementation(
        &self,
        file_path: &Path,
        position: Position,
    ) -> Result<Option<GotoDefinitionResponse>, LspError> {
        let Some((uri, _, targets)) = self.resolve_file_servers(file_path) else {
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
        let servers = self.server_views().await;
        for (config, root_path) in targets {
            if let Some(view) = servers.get(&ServerKey {
                server_name: config.name,
                root_path,
            }) && view.status() == ServerStatus::Running
                && view.supports("implementationProvider")
                && let Some(client) = view.client()
                && let Ok(Some(response)) = client
                    .send_request::<_, Option<GotoDefinitionResponse>>(
                        "textDocument/implementation",
                        params.clone(),
                    )
                    .await
            {
                return Ok(Some(response));
            }
        }
        Ok(None)
    }

    /// `textDocument/prepareRename`: the range and placeholder of the symbol
    /// at `position`, `Ok(None)` when the server declines or lacks prepare.
    ///
    /// # Errors
    /// Returns transport or protocol errors.
    pub async fn prepare_rename(
        &self,
        file_path: &Path,
        position: Position,
    ) -> Result<Option<PrepareRenameResponse>, LspError> {
        let Some((uri, _, targets)) = self.resolve_file_servers(file_path) else {
            return Ok(None);
        };
        let servers = self.server_views().await;
        for (config, root_path) in targets {
            if let Some(view) = servers.get(&ServerKey {
                server_name: config.name,
                root_path,
            }) && view.status() == ServerStatus::Running
                && view.flag("renameProvider", "prepareProvider")
                && let Some(client) = view.client()
            {
                return client
                    .send_request(
                        "textDocument/prepareRename",
                        TextDocumentPositionParams {
                            text_document: TextDocumentIdentifier { uri },
                            position,
                        },
                    )
                    .await;
            }
        }
        Ok(None)
    }

    /// `textDocument/rename` from the first server that provides it.
    ///
    /// # Errors
    /// Returns transport or protocol errors (including the server refusing
    /// the new name).
    pub async fn rename(
        &self,
        file_path: &Path,
        position: Position,
        new_name: String,
    ) -> Result<Option<WorkspaceEdit>, LspError> {
        let Some((uri, _, targets)) = self.resolve_file_servers(file_path) else {
            return Ok(None);
        };
        let servers = self.server_views().await;
        for (config, root_path) in targets {
            if let Some(view) = servers.get(&ServerKey {
                server_name: config.name,
                root_path,
            }) && view.status() == ServerStatus::Running
                && view.supports("renameProvider")
                && let Some(client) = view.client()
            {
                return client
                    .send_request(
                        "textDocument/rename",
                        RenameParams {
                            text_document_position: TextDocumentPositionParams {
                                text_document: TextDocumentIdentifier { uri },
                                position,
                            },
                            new_name,
                            work_done_progress_params: WorkDoneProgressParams::default(),
                        },
                    )
                    .await;
            }
        }
        Ok(None)
    }

    /// `codeAction/resolve` for actions listed without their edit.
    ///
    /// # Errors
    /// Returns transport or protocol errors.
    pub async fn resolve_code_action(
        &self,
        file_path: &Path,
        action: CodeAction,
    ) -> Result<CodeAction, LspError> {
        if action.edit.is_some() || action.data.is_none() {
            return Ok(action);
        }
        let Some((_, _, targets)) = self.resolve_file_servers(file_path) else {
            return Ok(action);
        };
        let servers = self.server_views().await;
        for (config, root_path) in targets {
            if let Some(view) = servers.get(&ServerKey {
                server_name: config.name,
                root_path,
            }) && view.status() == ServerStatus::Running
                && view.flag("codeActionProvider", "resolveProvider")
                && let Some(client) = view.client()
            {
                return client.send_request("codeAction/resolve", action).await;
            }
        }
        Ok(action)
    }

    /// Requests hover information at a position, stacking hover contents if multiple servers respond.
    ///
    /// # Errors
    /// Returns transport, serialization, protocol or lifecycle errors.
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

        let servers = self.server_views().await;
        let mut contents = Vec::new();
        let mut range = None;

        for (config, root_path) in server_targets {
            let key = ServerKey {
                server_name: config.name,
                root_path,
            };

            if let Some(instance) = servers.get(&key)
                && instance.status() == ServerStatus::Running
                && instance.supports("hoverProvider")
                && let Some(client) = instance.client()
                && let Ok(Some(hover)) = client.hover(params.clone()).await
            {
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
    ///
    /// # Errors
    /// Returns transport, serialization, protocol or lifecycle errors.
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

        let servers = self.server_views().await;
        for (config, root_path) in server_targets {
            let key = ServerKey {
                server_name: config.name,
                root_path,
            };

            if let Some(instance) = servers.get(&key)
                && instance.status() == ServerStatus::Running
                && instance.supports("definitionProvider")
                && let Some(client) = instance.client()
                && let Ok(Some(res)) = client.goto_definition(params.clone()).await
            {
                return Ok(Some(res));
            }
        }

        Ok(None)
    }

    /// Requests signature help only from servers advertising that provider.
    ///
    /// # Errors
    /// Returns transport or protocol errors.
    pub async fn signature_help(
        &self,
        file_path: &Path,
        position: Position,
    ) -> Result<Option<lsp_types::SignatureHelp>, LspError> {
        let Some((uri, _, targets)) = self.resolve_file_servers(file_path) else {
            return Ok(None);
        };
        let servers = self.server_views().await;
        for (config, root_path) in targets {
            if let Some(instance) = servers.get(&ServerKey {
                server_name: config.name,
                root_path,
            }) && instance.supports("signatureHelpProvider")
                && instance.status() == ServerStatus::Running
                && let Some(client) = instance.client()
            {
                return client
                    .send_request(
                        "textDocument/signatureHelp",
                        serde_json::json!({"textDocument":{"uri":uri},"position":position}),
                    )
                    .await;
            }
        }
        Ok(None)
    }

    /// Requests references for a position in a file.
    ///
    /// # Errors
    /// Returns transport, serialization, protocol or lifecycle errors.
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

        let servers = self.server_views().await;
        let mut results = Vec::new();

        for (config, root_path) in server_targets {
            let key = ServerKey {
                server_name: config.name,
                root_path,
            };

            if let Some(instance) = servers.get(&key)
                && instance.status() == ServerStatus::Running
                && instance.supports("referencesProvider")
                && let Some(client) = instance.client()
                && let Ok(Some(locs)) = client.references(params.clone()).await
            {
                results.extend(locs);
            }
        }

        Ok(results)
    }

    /// Formats the whole document.
    ///
    /// # Errors
    /// Returns transport, serialization, protocol or lifecycle errors.
    pub async fn formatting(
        &self,
        file_path: &Path,
        indentation: Indentation,
    ) -> Result<Option<Vec<TextEdit>>, LspError> {
        let Some((uri, _, server_targets)) = self.resolve_file_servers(file_path) else {
            return Ok(None);
        };

        let params = DocumentFormattingParams {
            text_document: TextDocumentIdentifier { uri },
            options: indentation.options(),
            work_done_progress_params: WorkDoneProgressParams::default(),
        };

        let servers = self.server_views().await;
        for (config, root_path) in server_targets {
            let key = ServerKey {
                server_name: config.name,
                root_path,
            };

            if let Some(instance) = servers.get(&key)
                && instance.status() == ServerStatus::Running
                && instance.supports("documentFormattingProvider")
                && let Some(client) = instance.client()
                && let Ok(Some(edits)) = client.formatting(params.clone()).await
            {
                return Ok(Some(edits));
            }
        }

        Ok(None)
    }

    /// Formats a range in the document.
    ///
    /// # Errors
    /// Returns transport, serialization, protocol or lifecycle errors.
    pub async fn range_formatting(
        &self,
        file_path: &Path,
        range: lsp_types::Range,
        indentation: Indentation,
    ) -> Result<Option<Vec<TextEdit>>, LspError> {
        let Some((uri, _, server_targets)) = self.resolve_file_servers(file_path) else {
            return Ok(None);
        };

        let params = DocumentRangeFormattingParams {
            text_document: TextDocumentIdentifier { uri },
            range,
            options: indentation.options(),
            work_done_progress_params: WorkDoneProgressParams::default(),
        };

        let servers = self.server_views().await;
        for (config, root_path) in server_targets {
            let key = ServerKey {
                server_name: config.name,
                root_path,
            };

            if let Some(instance) = servers.get(&key)
                && instance.status() == ServerStatus::Running
                && instance.supports("documentRangeFormattingProvider")
                && let Some(client) = instance.client()
                && let Ok(Some(edits)) = client.range_formatting(params.clone()).await
            {
                return Ok(Some(edits));
            }
        }

        Ok(None)
    }

    /// Requests code actions for a range in the document.
    ///
    /// # Errors
    /// Returns transport, serialization, protocol or lifecycle errors.
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

        let servers = self.server_views().await;
        let mut results = Vec::new();

        for (config, root_path) in server_targets {
            let key = ServerKey {
                server_name: config.name,
                root_path,
            };

            if let Some(instance) = servers.get(&key)
                && instance.status() == ServerStatus::Running
                && instance.supports("codeActionProvider")
                && let Some(client) = instance.client()
                && let Ok(Some(res)) = client.code_action(params.clone()).await
            {
                results.extend(res);
            }
        }

        Ok(results)
    }

    /// Requests symbols in the document.
    ///
    /// # Errors
    /// Returns transport, serialization, protocol or lifecycle errors.
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

        let servers = self.server_views().await;
        for (config, root_path) in server_targets {
            let key = ServerKey {
                server_name: config.name,
                root_path,
            };

            if let Some(instance) = servers.get(&key)
                && instance.status() == ServerStatus::Running
                && instance.supports("documentSymbolProvider")
                && let Some(client) = instance.client()
                && let Ok(Some(res)) = client.document_symbol(params.clone()).await
            {
                return Ok(Some(res));
            }
        }

        Ok(None)
    }

    /// Requests workspace symbols across all running language servers.
    ///
    /// # Errors
    /// Returns transport, serialization, protocol or lifecycle errors.
    pub async fn workspace_symbols(&self, query: &str) -> Result<Vec<SymbolInformation>, LspError> {
        let servers = self.server_views().await;
        let mut results = Vec::new();

        for instance in servers.values() {
            if instance.status() == ServerStatus::Running
                && instance.supports("workspaceSymbolProvider")
                && let Some(client) = instance.client()
                && let Ok(Some(symbols)) = client.workspace_symbols(query).await
            {
                results.extend(symbols);
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
            } else if matches!(
                instance.status(),
                ServerStatus::Running | ServerStatus::Crashed
            ) {
                let _ = instance.check_and_recover().await;
            }
        }
    }

    /// Shuts down all language servers cleanly.
    pub async fn shutdown_all(&self) {
        let mut servers = self.servers.write().await;
        for instance in servers.values_mut() {
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
