//! One asynchronous language service per window. GPUI only exchanges Rope
//! snapshots and bounded results; no process, RPC or diagnostic indexing on UI.

#[cfg(test)]
#[path = "lsp_tests.rs"]
mod tests;

use crate::assist::{Completion, CompletionItem, Diagnostic};
use crate::window::{ForgeWindow, NotificationLevel, Picker, PickerKind};
use forge_buffer::Edit;
use forge_gui::config::Config;
use forge_gui::i18n::{tr, trf};
use forge_lsp::{
    LanguageRegistry, LspManager, offset_to_position, path_to_uri, position_to_offset,
    snapshot_delta, uri_to_path,
};
use gpui::Context;
use ropey::Rope;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

/// Most completion candidates handed to the popup after client-side filtering.
const MAX_COMPLETIONS: usize = 200;
/// Most locations (references, definitions) listed at once.
const MAX_LOCATIONS: usize = 500;
/// Files larger than this are not read for location previews.
const MAX_PREVIEW_FILE: u64 = 4 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Feature {
    Completion,
    /// `completionItem/resolve` of an item already inserted.
    Resolve,
    Hover,
    Definition,
    Implementation,
    References,
    Signature,
    Problems,
    Format,
    CodeActions,
    /// `codeAction/resolve` of a chosen action.
    ResolveAction,
    PrepareRename,
    Rename,
    /// Context for `agent.investigate` from a diagnostic.
    Investigate,
}

/// Extra input a feature needs beyond the document position.
#[derive(Clone, Default)]
pub enum Payload {
    #[default]
    None,
    /// Completion typed after a possible trigger character.
    Trigger(char),
    Resolve(Box<lsp_types::CompletionItem>),
    Action(Box<lsp_types::CodeAction>),
    Rename(String),
    /// Format, then let the window save the file.
    FormatAndSave,
}

#[derive(Clone)]
struct Snapshot {
    path: PathBuf,
    rope: Rope,
    version: u64,
    cursor: usize,
    /// Other end of the primary selection (equals `cursor` when empty).
    anchor: usize,
    dirty: bool,
    first_line: usize,
    last_line: usize,
}

#[derive(Clone)]
struct Request {
    feature: Feature,
    snapshot: Snapshot,
    payload: Payload,
}

/// A request that is not tied to an open document (MCP tools).
#[derive(Clone)]
pub(crate) enum Query {
    WorkspaceSymbols(String),
}

#[derive(Default)]
struct State {
    documents: HashMap<u64, Snapshot>,
    requests: HashMap<(u64, Feature), Request>,
    queries: Vec<(u64, Query)>,
    watched: HashMap<PathBuf, bool>,
    shutdown: bool,
}

#[derive(Clone)]
pub struct LspHandle {
    tab: u64,
    state: Arc<Mutex<State>>,
}

impl LspHandle {
    pub fn request(&self, feature: Feature, version: u64, cursor: usize, rope: Rope) {
        self.request_with(feature, Payload::None, version, cursor, rope);
    }

    pub fn request_with(
        &self,
        feature: Feature,
        payload: Payload,
        version: u64,
        cursor: usize,
        rope: Rope,
    ) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(mut snapshot) = state.documents.get(&self.tab).cloned() else {
            return;
        };
        if snapshot.version != version {
            snapshot.dirty = true;
        }
        snapshot.rope = rope;
        snapshot.version = version;
        snapshot.cursor = cursor;
        if !matches!(feature, Feature::CodeActions) {
            snapshot.anchor = cursor;
        }
        state.requests.insert(
            (self.tab, feature),
            Request {
                feature,
                snapshot,
                payload,
            },
        );
    }
}

pub struct LspService {
    state: Arc<Mutex<State>>,
    results: mpsc::Receiver<Update>,
    configuration: (
        forge_gui::config::LspConfig,
        Vec<forge_lsp::LanguageDefinition>,
    ),
    /// Shared with the worker; read on the UI thread for MCP tools.
    diagnostics: forge_lsp::DiagnosticStore,
    next_query: u64,
}

impl Drop for LspService {
    fn drop(&mut self) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .shutdown = true;
    }
}

pub(crate) struct Problem {
    pub path: PathBuf,
    pub line: u32,
    pub column: u32,
    pub message: String,
}

/// A navigable place with the text of its line.
pub(crate) struct Location {
    pub path: PathBuf,
    pub position: lsp_types::Position,
    pub preview: String,
}

pub(crate) struct Investigation {
    pub hover: Option<String>,
    /// Definition of the symbol under the cursor: path, first line and text.
    pub definition: Option<(PathBuf, u32, String)>,
}

pub(crate) enum Response {
    Completion(Completion),
    Info(String),
    Locations(Vec<Location>),
    Problems(Vec<Problem>),
    /// Extra edits of a resolved completion, against the request's revision.
    Resolved(Vec<Edit>),
    /// Char-range edits against the request's revision; `save` afterwards.
    Edits {
        edits: Vec<Edit>,
        save: bool,
    },
    CodeActions(Vec<lsp_types::CodeActionOrCommand>),
    /// Text edits per file with the title of the operation that produced them.
    WorkspaceEdit {
        title: String,
        edits: forge_lsp::workspace_edit::DocumentEdits,
    },
    RenamePlaceholder(String),
    Investigate(Investigation),
}

pub(crate) enum Update {
    Diagnostics {
        tab: u64,
        version: u64,
        diagnostics: Vec<Diagnostic>,
        counts: (usize, usize, usize),
    },
    Response {
        tab: u64,
        path: PathBuf,
        version: u64,
        cursor: usize,
        response: Response,
    },
    Query {
        id: u64,
        result: Result<serde_json::Value, String>,
    },
    Status(String),
}

impl LspService {
    pub(crate) fn file_changed(&self, path: PathBuf, removed: bool) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.watched.len() < 1024 {
            state.watched.insert(path, removed);
        }
    }

    /// Diagnostics of every document any server reported on, for MCP.
    pub(crate) fn diagnostics(&self) -> &forge_lsp::DiagnosticStore {
        &self.diagnostics
    }

    /// Queues a workspace query; the answer arrives as [`Update::Query`].
    pub(crate) fn query(&mut self, query: Query) -> u64 {
        self.next_query += 1;
        let id = self.next_query;
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .queries
            .push((id, query));
        id
    }

    fn new(config: &Config) -> Self {
        let configuration = (config.lsp.clone(), config.languages.clone());
        let state = Arc::new(Mutex::new(State::default()));
        let (tx, results) = mpsc::sync_channel(128);
        let diagnostics = forge_lsp::DiagnosticStore::new();
        let worker_state = state.clone();
        let worker_config = configuration.clone();
        let worker_diagnostics = diagnostics.clone();
        std::thread::Builder::new()
            .name("forge-lsp".into())
            .spawn(move || {
                if let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    runtime.block_on(run(worker_state, tx, worker_config, worker_diagnostics));
                }
            })
            .expect("spawn language service");
        Self {
            state,
            results,
            configuration,
            diagnostics,
            next_query: 0,
        }
    }
}

struct OpenDocument {
    snapshot: Snapshot,
    observed: u64,
    changed: Instant,
    opened: bool,
    retry: Instant,
}

struct ActiveRequest {
    snapshot: Snapshot,
    task: tokio::task::JoinHandle<()>,
}

/// Pull diagnostics of one document: the revision asked for, the request
/// in flight (answering whether anything was reported) and, while a
/// freshly started server answers with nothing, when to ask again.
struct Pull {
    version: u64,
    task: Option<tokio::task::JoinHandle<bool>>,
    attempts: u32,
    retry: Instant,
}

/// Empty pull answers are retried this many times with doubling delays
/// (0.5 s … 16 s) so a server still indexing gets to report.
const PULL_RETRIES: u32 = 6;

fn same_position(a: &Snapshot, b: &Snapshot) -> bool {
    a.path == b.path && a.version == b.version && a.cursor == b.cursor
}

/// Whether a request outlives cursor and revision changes: resolving an
/// accepted completion happens after its insertion moved both.
fn detached(feature: Feature) -> bool {
    matches!(feature, Feature::Resolve)
}

fn cancel_stale_requests(
    tasks: &mut HashMap<(u64, Feature), ActiveRequest>,
    snapshots: &HashMap<u64, Snapshot>,
) {
    tasks.retain(|(id, feature), request| {
        let current = snapshots.get(id);
        let valid = if detached(*feature) {
            current.is_some_and(|snapshot| snapshot.path == request.snapshot.path)
        } else {
            current.is_some_and(|snapshot| same_position(snapshot, &request.snapshot))
        };
        if !valid {
            request.task.abort();
            return false;
        }
        !request.task.is_finished()
    });
}

async fn sync(
    manager: &LspManager,
    old: Option<&Snapshot>,
    new: &Snapshot,
) -> Result<(), forge_lsp::LspError> {
    if let Some(old) = old {
        if old.version != new.version {
            let edits = snapshot_delta(&old.rope, &new.rope);
            manager
                .change_document_versioned(
                    &new.path,
                    &edits,
                    &old.rope,
                    new.rope.to_string(),
                    new.version,
                )
                .await?;
        }
        if old.dirty && !new.dirty {
            manager
                .save_document(&new.path, Some(new.rope.to_string()))
                .await?;
        }
    } else {
        manager
            .open_document_versioned(&new.path, new.rope.to_string(), new.version)
            .await?;
    }
    Ok(())
}

fn language_manager(
    config: &forge_gui::config::LspConfig,
    languages: &[forge_lsp::LanguageDefinition],
    diagnostics: forge_lsp::DiagnosticStore,
) -> Arc<LspManager> {
    let mut registry = LanguageRegistry::new();
    for language in languages {
        registry.register(language.clone());
    }
    let mut manager = LspManager::new(registry, diagnostics);
    manager.set_idle_timeout(Duration::from_secs(config.idle_shutdown_secs.max(1)));
    Arc::new(manager)
}

#[allow(clippy::too_many_lines)]
async fn run(
    state: Arc<Mutex<State>>,
    results: mpsc::SyncSender<Update>,
    configuration: (
        forge_gui::config::LspConfig,
        Vec<forge_lsp::LanguageDefinition>,
    ),
    diagnostics: forge_lsp::DiagnosticStore,
) {
    let manager = language_manager(&configuration.0, &configuration.1, diagnostics);
    let mut documents: HashMap<u64, OpenDocument> = HashMap::new();
    let mut tasks: HashMap<(u64, Feature), ActiveRequest> = HashMap::new();
    let mut pulls: HashMap<u64, Pull> = HashMap::new();
    let mut ticker = tokio::time::interval(Duration::from_millis(25));
    let mut maintenance = Instant::now();
    let mut published = HashMap::new();
    loop {
        ticker.tick().await;
        let (snapshots, requests, queries, watched, shutdown) = {
            let mut state = state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (
                state.documents.clone(),
                std::mem::take(&mut state.requests),
                std::mem::take(&mut state.queries),
                std::mem::take(&mut state.watched),
                state.shutdown,
            )
        };
        if shutdown {
            break;
        }
        manager.watched_files(watched).await;
        sync_documents(
            &manager,
            &snapshots,
            &mut documents,
            &configuration.0,
            &results,
        )
        .await;
        cancel_stale_requests(&mut tasks, &snapshots);
        for ((id, feature), request) in requests {
            let current = snapshots.get(&id);
            let valid = if detached(feature) {
                current.is_some_and(|snapshot| snapshot.path == request.snapshot.path)
            } else {
                current.is_some_and(|snapshot| same_position(snapshot, &request.snapshot))
            };
            if !valid {
                continue;
            }
            if let Some(task) = tasks.remove(&(id, feature)) {
                task.task.abort();
            }
            if !detached(feature) {
                // Flush the exact revision the answer must refer to.
                let old = documents
                    .get(&id)
                    .filter(|doc| doc.opened)
                    .map(|doc| &doc.snapshot);
                if let Err(error) = manager
                    .open_document_versioned(
                        &request.snapshot.path,
                        request.snapshot.rope.to_string(),
                        request.snapshot.version,
                    )
                    .await
                {
                    let _ = results.try_send(Update::Status(format!("LSP: {error}")));
                    continue;
                }
                if let Err(error) = sync(&manager, old, &request.snapshot).await {
                    let _ = results.try_send(Update::Status(format!("LSP: {error}")));
                    continue;
                }
                if let Some(doc) = documents.get_mut(&id) {
                    doc.snapshot = request.snapshot.clone();
                    doc.opened = true;
                }
            }
            let manager = manager.clone();
            let results = results.clone();
            let snapshot = request.snapshot.clone();
            let task = tokio::spawn(async move {
                match respond(&manager, &request).await {
                    Ok(Some(response)) => {
                        let _ = results.try_send(Update::Response {
                            tab: id,
                            path: request.snapshot.path,
                            version: request.snapshot.version,
                            cursor: request.snapshot.cursor,
                            response,
                        });
                    }
                    Ok(None) => {}
                    Err(error) => {
                        let _ = results.try_send(Update::Status(format!("LSP: {error}")));
                    }
                }
            });
            tasks.insert((id, feature), ActiveRequest { snapshot, task });
        }
        for (id, query) in queries {
            let manager = manager.clone();
            let results = results.clone();
            tokio::spawn(async move {
                let result = answer_query(&manager, query).await;
                let _ = results.try_send(Update::Query { id, result });
            });
        }
        schedule_pulls(&manager, &documents, &mut pulls);
        publish_diagnostics(&manager, &snapshots, &results, &mut published);
        if maintenance.elapsed() >= Duration::from_secs(1) {
            manager.maintenance().await;
            maintenance = Instant::now();
        }
    }
    for task in tasks.into_values() {
        task.task.abort();
    }
    for pull in pulls.into_values() {
        if let Some(task) = pull.task {
            task.abort();
        }
    }
    manager.shutdown_all().await;
}

async fn sync_documents(
    manager: &LspManager,
    snapshots: &HashMap<u64, Snapshot>,
    documents: &mut HashMap<u64, OpenDocument>,
    config: &forge_gui::config::LspConfig,
    results: &mpsc::SyncSender<Update>,
) {
    let closed: Vec<_> = documents
        .keys()
        .filter(|id| !snapshots.contains_key(id))
        .copied()
        .collect();
    for id in closed {
        if let Some(doc) = documents.remove(&id) {
            let _ = manager.close_document(&doc.snapshot.path).await;
        }
    }
    for (id, snapshot) in snapshots {
        if documents
            .get(id)
            .is_some_and(|doc| doc.snapshot.path != snapshot.path)
            && let Some(doc) = documents.remove(id)
        {
            let _ = manager.close_document(&doc.snapshot.path).await;
        }
        let doc = documents.entry(*id).or_insert_with(|| OpenDocument {
            snapshot: snapshot.clone(),
            observed: snapshot.version,
            changed: Instant::now(),
            opened: false,
            retry: Instant::now(),
        });
        if doc.observed != snapshot.version {
            doc.observed = snapshot.version;
            doc.changed = Instant::now();
        }
        let wait = if doc.opened {
            config.debounce_ms
        } else {
            config.startup_ms
        };
        let pending = !doc.opened
            || doc.snapshot.version != snapshot.version
            || (doc.snapshot.dirty && !snapshot.dirty);
        if pending
            && doc.changed.elapsed() >= Duration::from_millis(wait)
            && Instant::now() >= doc.retry
        {
            match sync(manager, doc.opened.then_some(&doc.snapshot), snapshot).await {
                Ok(()) => {
                    doc.snapshot = snapshot.clone();
                    doc.opened = true;
                }
                Err(error) => {
                    doc.retry = Instant::now() + Duration::from_secs(5);
                    let _ = results.try_send(Update::Status(format!("LSP: {error}")));
                }
            }
        }
    }
}

/// One `textDocument/diagnostic` in flight per document for the revision
/// the server has; servers without pull support answer nothing and the
/// task ends at once. An empty answer right after opening is retried with
/// backoff: rust-analyzer reports nothing until the crate is indexed.
fn schedule_pulls(
    manager: &Arc<LspManager>,
    documents: &HashMap<u64, OpenDocument>,
    pulls: &mut HashMap<u64, Pull>,
) {
    pulls.retain(|id, pull| {
        let keep = documents.contains_key(id);
        if !keep && let Some(task) = &pull.task {
            task.abort();
        }
        keep
    });
    let now = Instant::now();
    for (id, doc) in documents {
        if !doc.opened {
            continue;
        }
        let version = doc.snapshot.version;
        let pull = pulls.entry(*id).or_insert_with(|| Pull {
            version,
            task: None,
            attempts: 0,
            retry: now,
        });
        if pull.version != version {
            if let Some(task) = pull.task.take() {
                task.abort();
            }
            pull.version = version;
            pull.attempts = 0;
            pull.retry = now;
        }
        if let Some(task) = &pull.task {
            if !task.is_finished() {
                continue;
            }
            let reported = pull.task.take().and_then(finished_output).unwrap_or(false);
            if reported || pull.attempts >= PULL_RETRIES {
                pull.attempts = u32::MAX;
                continue;
            }
            pull.retry = now + Duration::from_millis(500 << pull.attempts);
            pull.attempts += 1;
        }
        if pull.attempts == u32::MAX || now < pull.retry {
            continue;
        }
        let manager = manager.clone();
        let path = doc.snapshot.path.clone();
        pull.task = Some(tokio::spawn(async move {
            manager
                .pull_diagnostics(&path, version)
                .await
                .is_ok_and(|reported| reported > 0)
        }));
    }
}

/// The output of a task that already finished, without awaiting.
fn finished_output(task: tokio::task::JoinHandle<bool>) -> Option<bool> {
    use std::task::{Context, Poll};
    let mut context = Context::from_waker(std::task::Waker::noop());
    let mut task = std::pin::pin!(task);
    match std::future::Future::poll(task.as_mut(), &mut context) {
        Poll::Ready(result) => result.ok(),
        Poll::Pending => None,
    }
}

fn publish_diagnostics(
    manager: &LspManager,
    snapshots: &HashMap<u64, Snapshot>,
    results: &mpsc::SyncSender<Update>,
    published: &mut HashMap<u64, (u64, usize, usize, u64)>,
) {
    published.retain(|tab, _| snapshots.contains_key(tab));
    for (tab, snapshot) in snapshots {
        let Some(uri) = path_to_uri(&snapshot.path) else {
            continue;
        };
        let key = (
            snapshot.version,
            snapshot.first_line,
            snapshot.last_line,
            manager.diagnostics().generation(),
        );
        if published.get(tab) == Some(&key) {
            continue;
        }
        if manager
            .diagnostics()
            .version_for_document(&uri)
            .is_some_and(|version| i32::try_from(snapshot.version).ok() != Some(version))
        {
            continue;
        }
        let diagnostics = manager
            .diagnostics()
            .for_line_range(
                &uri,
                u32::try_from(snapshot.first_line).unwrap_or(u32::MAX)
                    ..u32::try_from(snapshot.last_line).unwrap_or(u32::MAX),
            )
            .iter()
            .take(200)
            .map(|diag| crate::assist::from_lsp_diagnostic(diag, &snapshot.rope))
            .collect();
        let counts = manager.diagnostics().counts_for_document(&uri);
        if results
            .try_send(Update::Diagnostics {
                tab: *tab,
                version: snapshot.version,
                diagnostics,
                counts,
            })
            .is_ok()
        {
            published.insert(*tab, key);
        }
    }
}

async fn answer_query(manager: &LspManager, query: Query) -> Result<serde_json::Value, String> {
    match query {
        Query::WorkspaceSymbols(text) => {
            let symbols = manager
                .workspace_symbols(&text)
                .await
                .map_err(|error| error.to_string())?;
            let symbols: Vec<serde_json::Value> = symbols
                .into_iter()
                .take(MAX_LOCATIONS)
                .filter_map(|symbol| {
                    let path = uri_to_path(&symbol.location.uri)?;
                    Some(serde_json::json!({
                        "name": symbol.name,
                        "kind": format!("{:?}", symbol.kind),
                        "container": symbol.container_name,
                        "path": path,
                        "line": symbol.location.range.start.line + 1,
                        "column": symbol.location.range.start.character + 1,
                    }))
                })
                .collect();
            Ok(serde_json::json!({"symbols": symbols}))
        }
    }
}

#[allow(clippy::too_many_lines)]
async fn respond(
    manager: &LspManager,
    request: &Request,
) -> Result<Option<Response>, forge_lsp::LspError> {
    let snapshot = &request.snapshot;
    let position = offset_to_position(&snapshot.rope, snapshot.cursor);
    Ok(match request.feature {
        Feature::Completion => {
            let trigger = match request.payload {
                Payload::Trigger(character) => {
                    if !manager.completion_trigger(&snapshot.path, character).await {
                        return Ok(None);
                    }
                    Some(character)
                }
                _ => None,
            };
            let items = manager
                .completion(&snapshot.path, position, trigger)
                .await?;
            let start = prefix_start(&snapshot.rope, snapshot.cursor);
            let prefix = snapshot.rope.slice(start..snapshot.cursor).to_string();
            let items: Vec<_> = rank_completions(&items, &prefix)
                .into_iter()
                .map(|item| completion_item(item, &snapshot.rope, start, snapshot.cursor))
                .collect();
            (!items.is_empty()).then(|| {
                Response::Completion(Completion {
                    items,
                    index: 0,
                    start,
                    prefix,
                })
            })
        }
        Feature::Resolve => {
            let Payload::Resolve(item) = &request.payload else {
                return Ok(None);
            };
            let resolved = manager
                .resolve_completion(&snapshot.path, (**item).clone())
                .await?;
            let edits = resolved.additional_text_edits.unwrap_or_default();
            (!edits.is_empty())
                .then(|| Response::Resolved(text_edits_to_edits(&snapshot.rope, &edits)))
        }
        Feature::Hover => manager
            .hover(&snapshot.path, position)
            .await?
            .map(|hover| Response::Info(hover_text(hover.contents))),
        Feature::Definition => manager
            .goto_definition(&snapshot.path, position)
            .await?
            .map(|definition| {
                Response::Locations(locations(definition_targets(definition), snapshot))
            }),
        Feature::Implementation => manager
            .goto_implementation(&snapshot.path, position)
            .await?
            .map(|definition| {
                Response::Locations(locations(definition_targets(definition), snapshot))
            }),
        Feature::References => {
            let found = manager.references(&snapshot.path, position, true).await?;
            Some(Response::Locations(locations(
                found
                    .into_iter()
                    .map(|location| (location.uri, location.range.start))
                    .collect(),
                snapshot,
            )))
        }
        Feature::Signature => manager
            .signature_help(&snapshot.path, position)
            .await?
            .map(|info| {
                Response::Info(
                    info.signatures
                        .iter()
                        .map(|signature| signature.label.as_str())
                        .collect::<Vec<_>>()
                        .join("\n\n"),
                )
            }),
        Feature::Problems => Some(Response::Problems(
            manager
                .diagnostics()
                .all_diagnostics()
                .into_iter()
                .flat_map(|(uri, items)| {
                    let path = uri_to_path(&uri);
                    items.into_iter().filter_map(move |diag| {
                        Some(Problem {
                            path: path.clone()?,
                            line: diag.range.start.line,
                            column: diag.range.start.character,
                            message: diag.message,
                        })
                    })
                })
                .take(1000)
                .collect(),
        )),
        Feature::Format => {
            let save = matches!(request.payload, Payload::FormatAndSave);
            let indentation = forge_lsp::Indentation::default();
            let edits = match manager.formatting(&snapshot.path, indentation).await {
                Ok(edits) => edits.unwrap_or_default(),
                // A save must not be lost to a formatter failure.
                Err(error) if save => {
                    tracing::warn!(%error, "format on save skipped");
                    Vec::new()
                }
                Err(error) => return Err(error),
            };
            Some(Response::Edits {
                edits: text_edits_to_edits(&snapshot.rope, &edits),
                save,
            })
        }
        Feature::CodeActions => {
            let (from, to) = (
                snapshot.cursor.min(snapshot.anchor),
                snapshot.cursor.max(snapshot.anchor),
            );
            let range = lsp_types::Range::new(
                offset_to_position(&snapshot.rope, from),
                offset_to_position(&snapshot.rope, to),
            );
            let diagnostics = path_to_uri(&snapshot.path)
                .map(|uri| {
                    manager
                        .diagnostics()
                        .for_line_range(&uri, range.start.line..range.end.line + 1)
                        .into_iter()
                        .filter(|diag| {
                            diag.range.start <= range.end && diag.range.end >= range.start
                        })
                        .collect()
                })
                .unwrap_or_default();
            let actions = manager
                .code_actions(&snapshot.path, range, diagnostics)
                .await?;
            Some(Response::CodeActions(actions))
        }
        Feature::ResolveAction => {
            let Payload::Action(action) = &request.payload else {
                return Ok(None);
            };
            let resolved = manager
                .resolve_code_action(&snapshot.path, (**action).clone())
                .await?;
            match resolved.edit {
                Some(edit) => Some(workspace_edit_response(resolved.title, &edit)),
                None => Some(Response::Info(trf(
                    "{} needs a server command, which Forge does not run",
                    &[&resolved.title],
                ))),
            }
        }
        Feature::PrepareRename => {
            let placeholder = match manager.prepare_rename(&snapshot.path, position).await? {
                Some(lsp_types::PrepareRenameResponse::RangeWithPlaceholder {
                    placeholder,
                    ..
                }) => placeholder,
                Some(lsp_types::PrepareRenameResponse::Range(range)) => snapshot
                    .rope
                    .slice(forge_lsp::sync::lsp_range_to_range(&snapshot.rope, &range))
                    .to_string(),
                Some(lsp_types::PrepareRenameResponse::DefaultBehavior { .. }) | None => {
                    let (start, word) = crate::assist::word_prefix(&snapshot.rope, snapshot.cursor);
                    let mut end = snapshot.cursor;
                    while end < snapshot.rope.len_chars()
                        && (snapshot.rope.char(end).is_alphanumeric()
                            || snapshot.rope.char(end) == '_')
                    {
                        end += 1;
                    }
                    let _ = start;
                    word + &snapshot.rope.slice(snapshot.cursor..end).to_string()
                }
            };
            Some(Response::RenamePlaceholder(placeholder))
        }
        Feature::Rename => {
            let Payload::Rename(new_name) = &request.payload else {
                return Ok(None);
            };
            manager
                .rename(&snapshot.path, position, new_name.clone())
                .await?
                .map(|edit| workspace_edit_response(trf("Rename to {}", &[&new_name]), &edit))
        }
        Feature::Investigate => {
            let hover = manager
                .hover(&snapshot.path, position)
                .await
                .ok()
                .flatten()
                .map(|hover| hover_text(hover.contents));
            let definition = manager
                .goto_definition(&snapshot.path, position)
                .await
                .ok()
                .flatten()
                .and_then(|definition| definition_targets(definition).into_iter().next())
                .and_then(|(uri, position)| {
                    let path = uri_to_path(&uri)?;
                    let text = lines_at(&path, snapshot, position.line, 20);
                    Some((path, position.line, text))
                });
            Some(Response::Investigate(Investigation { hover, definition }))
        }
    })
}

fn workspace_edit_response(title: String, edit: &lsp_types::WorkspaceEdit) -> Response {
    match forge_lsp::workspace_edit::text_edits(edit) {
        Ok(edits) => Response::WorkspaceEdit { title, edits },
        Err(error) => Response::Info(error),
    }
}

fn definition_targets(
    definition: lsp_types::GotoDefinitionResponse,
) -> Vec<(lsp_types::Uri, lsp_types::Position)> {
    match definition {
        lsp_types::GotoDefinitionResponse::Scalar(location) => {
            vec![(location.uri, location.range.start)]
        }
        lsp_types::GotoDefinitionResponse::Array(items) => items
            .into_iter()
            .map(|location| (location.uri, location.range.start))
            .collect(),
        lsp_types::GotoDefinitionResponse::Link(items) => items
            .into_iter()
            .map(|link| (link.target_uri, link.target_selection_range.start))
            .collect(),
    }
}

/// Resolves URIs to paths and reads one preview line per location (the
/// live rope for the request's own file, disk otherwise, bounded).
fn locations(
    targets: Vec<(lsp_types::Uri, lsp_types::Position)>,
    snapshot: &Snapshot,
) -> Vec<Location> {
    let mut files: HashMap<PathBuf, Option<Rope>> = HashMap::new();
    let mut result = Vec::new();
    for (uri, position) in targets.into_iter().take(MAX_LOCATIONS) {
        let Some(path) = uri_to_path(&uri) else {
            continue;
        };
        let rope = files
            .entry(path.clone())
            .or_insert_with(|| file_rope(&path, snapshot));
        let preview = rope
            .as_ref()
            .and_then(|rope| {
                let line = position.line as usize;
                (line < rope.len_lines()).then(|| rope.line(line).to_string().trim().to_owned())
            })
            .unwrap_or_default();
        result.push(Location {
            path,
            position,
            preview,
        });
    }
    result
}

fn file_rope(path: &Path, snapshot: &Snapshot) -> Option<Rope> {
    if path == snapshot.path {
        return Some(snapshot.rope.clone());
    }
    let size = std::fs::metadata(path).ok()?.len();
    if size > MAX_PREVIEW_FILE {
        return None;
    }
    std::fs::read_to_string(path)
        .ok()
        .map(|text| Rope::from_str(&text))
}

fn lines_at(path: &Path, snapshot: &Snapshot, line: u32, count: usize) -> String {
    file_rope(path, snapshot)
        .map(|rope| {
            let start = (line as usize).min(rope.len_lines());
            let end = start.saturating_add(count).min(rope.len_lines());
            rope.slice(rope.line_to_char(start)..rope.line_to_char(end))
                .to_string()
        })
        .unwrap_or_default()
}

fn text_edits_to_edits(rope: &Rope, edits: &[lsp_types::TextEdit]) -> Vec<Edit> {
    edits
        .iter()
        .map(|edit| Edit {
            range: forge_lsp::sync::lsp_range_to_range(rope, &edit.range),
            text: edit.new_text.clone(),
        })
        .collect()
}

fn hover_text(contents: lsp_types::HoverContents) -> String {
    match contents {
        lsp_types::HoverContents::Markup(content) => content.value,
        lsp_types::HoverContents::Scalar(lsp_types::MarkedString::String(text)) => text,
        lsp_types::HoverContents::Scalar(lsp_types::MarkedString::LanguageString(text)) => {
            format!("```{}\n{}\n```", text.language, text.value)
        }
        lsp_types::HoverContents::Array(items) => items
            .into_iter()
            .map(|item| hover_text(lsp_types::HoverContents::Scalar(item)))
            .collect::<Vec<_>>()
            .join("\n\n"),
    }
}

fn prefix_start(rope: &Rope, cursor: usize) -> usize {
    let mut start = cursor;
    while start > 0 && (rope.char(start - 1).is_alphanumeric() || rope.char(start - 1) == '_') {
        start -= 1;
    }
    start
}

/// How well `candidate` matches what was typed: exact prefix, prefix
/// ignoring case, or a subsequence starting at a word boundary (`rdln` →
/// `read_line`, `hm` → `HashMap`, but not `re` → `unrelated`).
fn match_score(candidate: &str, prefix: &str) -> Option<u8> {
    if prefix.is_empty() {
        return Some(0);
    }
    if candidate.starts_with(prefix) {
        return Some(0);
    }
    let prefix_lower: Vec<char> = prefix.to_lowercase().chars().collect();
    let candidate: Vec<char> = candidate.chars().collect();
    let lower: Vec<char> = candidate.iter().flat_map(|c| c.to_lowercase()).collect();
    if lower.len() == candidate.len() && lower.starts_with(&prefix_lower) {
        return Some(1);
    }
    let mut wanted = 0;
    for (index, c) in lower.iter().enumerate() {
        if wanted < prefix_lower.len() && *c == prefix_lower[wanted] {
            let boundary = index == 0
                || matches!(candidate[index - 1], '_' | '-' | '.' | ':')
                || (candidate[index].is_uppercase() && candidate[index - 1].is_lowercase());
            if wanted > 0 || boundary {
                wanted += 1;
            }
        }
    }
    (wanted == prefix_lower.len()).then_some(2)
}

/// Servers answer with every candidate at the position (rust-analyzer does
/// not filter by prefix); the popup shows the ones matching what was typed,
/// best matches first, then the server's own order.
fn rank_completions<'a>(
    items: &'a [lsp_types::CompletionItem],
    prefix: &str,
) -> Vec<&'a lsp_types::CompletionItem> {
    let mut ranked: Vec<(u8, &lsp_types::CompletionItem)> = items
        .iter()
        .filter_map(|item| {
            let candidate = item.filter_text.as_deref().unwrap_or(&item.label);
            match_score(candidate, prefix).map(|score| (score, item))
        })
        .collect();
    ranked.sort_by(|(a_score, a), (b_score, b)| {
        a_score.cmp(b_score).then_with(|| {
            a.sort_text
                .as_deref()
                .unwrap_or(&a.label)
                .cmp(b.sort_text.as_deref().unwrap_or(&b.label))
        })
    });
    ranked
        .into_iter()
        .map(|(_, item)| item)
        .take(MAX_COMPLETIONS)
        .collect()
}

fn completion_item(
    item: &lsp_types::CompletionItem,
    rope: &Rope,
    start: usize,
    cursor: usize,
) -> CompletionItem {
    let mut result = crate::assist::from_lsp_completion_item(item);
    let (range, new_text) = match &item.text_edit {
        Some(lsp_types::CompletionTextEdit::Edit(edit)) => (
            forge_lsp::sync::lsp_range_to_range(rope, &edit.range),
            edit.new_text.clone(),
        ),
        Some(lsp_types::CompletionTextEdit::InsertAndReplace(edit)) => (
            forge_lsp::sync::lsp_range_to_range(rope, &edit.replace),
            edit.new_text.clone(),
        ),
        None => (
            start..cursor,
            item.insert_text.as_ref().unwrap_or(&item.label).clone(),
        ),
    };
    let text = if item.insert_text_format == Some(lsp_types::InsertTextFormat::SNIPPET) {
        let snippet = crate::snippet::parse(&new_text);
        let text = snippet.text.clone();
        if snippet.has_tabstops() {
            result.snippet = Some(snippet);
        }
        text
    } else {
        new_text
    };
    let mut edits = vec![Edit { range, text }];
    edits.extend(
        item.additional_text_edits
            .iter()
            .flatten()
            .map(|edit| Edit {
                range: forge_lsp::sync::lsp_range_to_range(rope, &edit.range),
                text: edit.new_text.clone(),
            }),
    );
    result.edits = Some(edits);
    // Only items the server can still enrich need a resolve round-trip.
    if item.data.is_some() && item.additional_text_edits.is_none() {
        result.lsp = Some(Box::new(item.clone()));
    }
    result
}

impl ForgeWindow {
    pub(crate) fn poll_lsp(&mut self) -> bool {
        if !self.config.lsp.enabled {
            self.lsp = None;
            let mut changed = false;
            for tab in &mut self.tabs {
                if let Some(editor) = tab.editor_mut() {
                    changed |= !editor.lsp_diagnostics.is_empty();
                    editor.lsp_handle = None;
                    editor.lsp_diagnostics.clear();
                    editor.lsp_counts = (0, 0, 0);
                }
            }
            return changed;
        }
        if self.lsp.as_ref().is_some_and(|service| {
            service.configuration.0 != self.config.lsp
                || service.configuration.1 != self.config.languages
        }) {
            self.lsp = None;
        }
        if self.lsp.is_none()
            && self.tabs.iter().any(|tab| {
                tab.editor()
                    .is_some_and(|editor| editor.path().is_some() && editor.large.is_none())
            })
        {
            self.lsp = Some(LspService::new(&self.config));
        }
        let Some(service) = &self.lsp else {
            return false;
        };
        let mut state = service
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.documents.clear();
        for tab in &mut self.tabs {
            let id = tab.id;
            if let Some(editor) = tab.editor_mut()
                && editor.large.is_none()
                && let Some(path) = editor.path()
            {
                let primary = editor.buffer.selections().primary();
                state.documents.insert(
                    id,
                    Snapshot {
                        path: path.into(),
                        rope: editor.buffer.rope(),
                        version: editor.buffer.version(),
                        cursor: primary.head,
                        anchor: primary.anchor,
                        dirty: editor.buffer.is_dirty(),
                        first_line: editor.scroll_line,
                        last_line: editor.scroll_line.saturating_add(editor.visible_rows + 1),
                    },
                );
                editor.lsp_handle = Some(LspHandle {
                    tab: id,
                    state: service.state.clone(),
                });
            }
        }
        drop(state);
        let updates: Vec<_> = service.results.try_iter().take(32).collect();
        self.apply_lsp_updates(updates)
    }

    #[allow(clippy::too_many_lines)]
    fn apply_lsp_updates(&mut self, updates: Vec<Update>) -> bool {
        let mut changed = false;
        for update in updates {
            match update {
                Update::Diagnostics {
                    tab,
                    version,
                    diagnostics,
                    counts,
                } => {
                    if let Some(editor) = self
                        .tabs
                        .iter_mut()
                        .find(|item| item.id == tab)
                        .and_then(crate::window::Tab::editor_mut)
                        && editor.buffer.version() == version
                    {
                        changed |=
                            editor.lsp_diagnostics != diagnostics || editor.lsp_counts != counts;
                        editor.lsp_diagnostics = diagnostics;
                        editor.lsp_counts = counts;
                    }
                }
                Update::Response {
                    tab,
                    path,
                    version,
                    cursor,
                    response,
                } => {
                    let Some(index) = self.tabs.iter().position(|item| item.id == tab) else {
                        continue;
                    };
                    // A resolve answers for the revision before the insertion.
                    let resolve = matches!(response, Response::Resolved(_));
                    if !self.tabs[index].editor().is_some_and(|editor| {
                        editor.path() == Some(path.as_path())
                            && (resolve
                                || (editor.buffer.version() == version
                                    && editor.buffer.selections().primary().head == cursor))
                    }) {
                        continue;
                    }
                    self.apply_lsp_response(index, tab, version, response);
                    changed = true;
                }
                Update::Query { id, result } => {
                    self.mcp_answer_lsp_query(id, result);
                }
                Update::Status(message) => self.notify_user(NotificationLevel::Warning, message),
            }
        }
        changed
    }

    #[allow(clippy::too_many_lines)]
    fn apply_lsp_response(&mut self, index: usize, tab: u64, version: u64, response: Response) {
        let active = self.active_tab().id == tab;
        match response {
            Response::Completion(completion) => {
                if let Some(editor) = self.tabs[index].editor_mut() {
                    editor.completion = Some(completion);
                }
            }
            Response::Resolved(edits) => {
                if let Some(editor) = self.tabs[index].editor_mut() {
                    editor.apply_resolved_completion(&edits, version);
                }
            }
            Response::Edits { edits, save } => {
                let Some(editor) = self.tabs[index].editor_mut() else {
                    return;
                };
                if !edits.is_empty() {
                    match editor.buffer.edit(edits, false) {
                        Ok(_) => editor.sync_syntax(),
                        Err(error) => {
                            self.notify_user(
                                NotificationLevel::Error,
                                trf("Edit failed: {}", &[&error]),
                            );
                            return;
                        }
                    }
                }
                if save {
                    self.lsp_save_after_format = Some(tab);
                }
            }
            Response::Info(text) if active && self.picker.is_none() => {
                self.picker = Some(Picker {
                    title: tr("Language server").into(),
                    items: vec![text],
                    index: 0,
                    kind: PickerKind::LspInfo,
                });
            }
            Response::Locations(locations) if active => {
                self.lsp_show_locations(locations);
            }
            Response::Problems(problems) if active => {
                self.lsp_problems = problems;
                self.picker = Some(Picker {
                    title: tr("Diagnostics (up to 1000)").into(),
                    items: self
                        .lsp_problems
                        .iter()
                        .map(|problem| {
                            format!(
                                "{}:{} · {}",
                                problem.path.display(),
                                problem.line + 1,
                                problem.message
                            )
                        })
                        .collect(),
                    index: 0,
                    kind: PickerKind::LspProblems,
                });
            }
            Response::CodeActions(actions) if active => {
                if actions.is_empty() {
                    self.notify_user(NotificationLevel::Info, tr("No code actions here"));
                    return;
                }
                self.picker = Some(Picker {
                    title: tr("Code actions").into(),
                    items: actions
                        .iter()
                        .map(|action| match action {
                            lsp_types::CodeActionOrCommand::CodeAction(action) => {
                                action.title.clone()
                            }
                            lsp_types::CodeActionOrCommand::Command(command) => {
                                command.title.clone()
                            }
                        })
                        .collect(),
                    index: 0,
                    kind: PickerKind::LspCodeActions,
                });
                self.lsp_code_actions = actions;
            }
            Response::WorkspaceEdit { title, edits } if active => {
                self.lsp_preview_workspace_edit(title, edits);
            }
            Response::RenamePlaceholder(placeholder) if active => {
                self.rename = Some(crate::window::TextPrompt {
                    title: tr("Rename symbol").into(),
                    value: placeholder,
                    kind: crate::window::PromptKind::LspRename,
                });
            }
            Response::Investigate(investigation) if active => {
                self.lsp_investigation = Some(investigation);
            }
            _ => {}
        }
    }

    pub(crate) fn lsp_feature(&mut self, feature: Feature, cx: &mut Context<Self>) {
        self.lsp_feature_with(feature, Payload::None, cx);
    }

    pub(crate) fn lsp_feature_with(
        &mut self,
        feature: Feature,
        payload: Payload,
        cx: &mut Context<Self>,
    ) {
        self.poll_lsp();
        if let Some(editor) = self.active_tab().editor()
            && let Some(handle) = &editor.lsp_handle
        {
            handle.request_with(
                feature,
                payload,
                editor.buffer.version(),
                editor.buffer.selections().primary().head,
                editor.buffer.rope(),
            );
        } else if self.config.lsp.enabled {
            self.notify_user(
                NotificationLevel::Info,
                tr("No language server for this file"),
            );
        }
        cx.notify();
    }

    /// One location jumps straight there; several are listed to pick from.
    fn lsp_show_locations(&mut self, locations: Vec<Location>) {
        match locations.len() {
            0 => self.notify_user(NotificationLevel::Info, tr("No locations found")),
            1 => {
                let location = &locations[0];
                self.lsp_navigation = Some((location.path.clone(), location.position));
            }
            _ => {
                let cwd = self.factory.cwd.clone();
                self.picker = Some(Picker {
                    title: trf("{} locations", &[&locations.len()]),
                    items: locations
                        .iter()
                        .map(|location| {
                            let path = location.path.strip_prefix(&cwd).unwrap_or(&location.path);
                            format!(
                                "{}:{} · {}",
                                path.display(),
                                location.position.line + 1,
                                location.preview
                            )
                        })
                        .collect(),
                    index: 0,
                    kind: PickerKind::LspLocations,
                });
                self.lsp_locations = locations;
            }
        }
    }

    pub(crate) fn lsp_location_pick(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(location) = self.lsp_locations.get(index) {
            self.lsp_navigation = Some((location.path.clone(), location.position));
            self.lsp_finish_navigation(cx);
        }
    }

    pub(crate) fn lsp_problem_pick(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(problem) = self.lsp_problems.get(index) {
            let path = problem.path.clone();
            let line = problem.line;
            let column = problem.column;
            self.lsp_navigation = Some((path, lsp_types::Position::new(line, column)));
            self.lsp_finish_navigation(cx);
        }
    }

    /// A chosen code action: resolve its edit (or apply it when listed).
    pub(crate) fn lsp_code_action_pick(&mut self, index: usize, cx: &mut Context<Self>) {
        if index >= self.lsp_code_actions.len() {
            return;
        }
        match self.lsp_code_actions.swap_remove(index) {
            lsp_types::CodeActionOrCommand::CodeAction(action) => match &action.edit {
                Some(edit) => match forge_lsp::workspace_edit::text_edits(edit) {
                    Ok(edits) => self.lsp_preview_workspace_edit(action.title, edits),
                    Err(error) => self.notify_user(NotificationLevel::Warning, error),
                },
                None => self.lsp_feature_with(
                    Feature::ResolveAction,
                    Payload::Action(Box::new(action)),
                    cx,
                ),
            },
            lsp_types::CodeActionOrCommand::Command(command) => self.notify_user(
                NotificationLevel::Info,
                trf(
                    "{} needs a server command, which Forge does not run",
                    &[&command.title],
                ),
            ),
        }
        self.lsp_code_actions.clear();
        cx.notify();
    }

    /// Lists what a workspace edit would touch; Enter applies, Esc cancels.
    /// Edits confined to the active file are applied at once.
    fn lsp_preview_workspace_edit(
        &mut self,
        title: String,
        edits: forge_lsp::workspace_edit::DocumentEdits,
    ) {
        let count = forge_lsp::workspace_edit::edit_count(&edits);
        if count == 0 {
            self.notify_user(NotificationLevel::Info, tr("Nothing to change"));
            return;
        }
        let active_path = self
            .active_tab()
            .editor()
            .and_then(|editor| editor.path())
            .map(Path::to_path_buf);
        let single_file = edits.len() == 1
            && edits
                .first()
                .and_then(|(uri, _)| uri_to_path(uri))
                .is_some_and(|path| Some(path) == active_path);
        if single_file {
            // Applied on the next poll, which has the window context.
            self.lsp_apply_edit = Some((title, edits));
            return;
        }
        let cwd = self.factory.cwd.clone();
        let mut items = vec![trf("Apply {} edits in {} files", &[&count, &edits.len()])];
        items.extend(edits.iter().map(|(uri, edits)| {
            let path = uri_to_path(uri).unwrap_or_else(|| PathBuf::from(uri.as_str()));
            let path = path.strip_prefix(&cwd).unwrap_or(&path);
            trf("{} · {} edits", &[&path.display(), &edits.len()])
        }));
        self.picker = Some(Picker {
            title: title.clone(),
            items,
            index: 0,
            kind: PickerKind::LspWorkspaceEdit,
        });
        self.lsp_workspace_edit = Some((title, edits));
    }

    pub(crate) fn lsp_workspace_edit_confirm(&mut self, cx: &mut Context<Self>) {
        if let Some((title, edits)) = self.lsp_workspace_edit.take() {
            self.lsp_apply_workspace_edit(&title, edits, cx);
        }
        cx.notify();
    }

    /// Applies text edits through buffer transactions: open files get one
    /// transaction each (undoable), other files are opened first so the
    /// user saves them knowingly. Nothing is written to disk here.
    fn lsp_apply_workspace_edit(
        &mut self,
        title: &str,
        edits: forge_lsp::workspace_edit::DocumentEdits,
        cx: &mut Context<Self>,
    ) {
        let mut files = 0usize;
        let mut applied = 0usize;
        let mut failures = Vec::new();
        let active = self.active_tab;
        for (uri, text_edits) in edits {
            let Some(path) = uri_to_path(&uri) else {
                failures.push(uri.as_str().to_owned());
                continue;
            };
            let path = std::fs::canonicalize(&path).unwrap_or(path);
            if !self.tabs.iter().any(|tab| {
                tab.editor()
                    .is_some_and(|editor| editor.path() == Some(path.as_path()))
            }) {
                self.open_file(&path, None, None, cx);
            }
            let Some(editor) = self
                .tabs
                .iter_mut()
                .filter_map(crate::window::Tab::editor_mut)
                .find(|editor| editor.path() == Some(path.as_path()) && editor.large.is_none())
            else {
                failures.push(path.display().to_string());
                continue;
            };
            let rope = editor.buffer.rope();
            let edits = text_edits_to_edits(&rope, &text_edits);
            match editor.buffer.edit(edits, false) {
                Ok(_) => {
                    editor.sync_syntax();
                    files += 1;
                    applied += text_edits.len();
                }
                Err(error) => failures.push(format!("{}: {error}", path.display())),
            }
        }
        self.activate_tab(active, cx);
        if failures.is_empty() {
            self.notify_user(
                NotificationLevel::Info,
                trf("{}: {} edits in {} files", &[&title, &applied, &files]),
            );
        } else {
            self.notify_user(
                NotificationLevel::Warning,
                trf("{}: could not edit {}", &[&title, &failures.join(", ")]),
            );
        }
    }

    pub(crate) fn lsp_rename_submit(&mut self, new_name: &str, cx: &mut Context<Self>) {
        if new_name.is_empty() {
            return;
        }
        self.lsp_feature_with(Feature::Rename, Payload::Rename(new_name.to_owned()), cx);
    }

    /// Work that needs the window context, deferred by the poll: opening
    /// files, applying edits, saving after a format, launching an agent.
    pub(crate) fn lsp_finish_pending(&mut self, cx: &mut Context<Self>) {
        self.lsp_finish_navigation(cx);
        if let Some((title, edits)) = self.lsp_apply_edit.take() {
            self.lsp_apply_workspace_edit(&title, edits, cx);
        }
        if let Some(tab) = self.lsp_save_after_format.take()
            && let Some(index) = self.tabs.iter().position(|item| item.id == tab)
        {
            let active = self.active_tab;
            self.activate_tab(index, cx);
            self.save_active_now(cx);
            self.activate_tab(active, cx);
        }
        if let Some(investigation) = self.lsp_investigation.take() {
            self.agent_investigate_diagnostic(investigation, cx);
        }
    }

    fn lsp_finish_navigation(&mut self, cx: &mut Context<Self>) {
        if let Some((path, position)) = self.lsp_navigation.take() {
            self.open_file(&path, None, None, cx);
            if let Some(editor) = self.active_tab_mut().editor_mut()
                && editor.path() == Some(path.as_path())
            {
                let offset = position_to_offset(&editor.buffer.rope(), position);
                editor
                    .buffer
                    .set_selections(forge_buffer::Selections::single(
                        forge_buffer::Selection::point(offset),
                    ));
                editor.follow_cursor();
            }
            cx.notify();
        }
    }

    /// `forge/diagnostics` for the MCP server: every document, or one path.
    pub(crate) fn mcp_diagnostics(&self, params: &serde_json::Value) -> serde_json::Value {
        let filter = params
            .get("path")
            .and_then(serde_json::Value::as_str)
            .map(|path| {
                let path = Path::new(path);
                if path.is_absolute() {
                    path.to_path_buf()
                } else {
                    self.factory.cwd.join(path)
                }
            })
            .map(|path| std::fs::canonicalize(&path).unwrap_or(path));
        let Some(service) = &self.lsp else {
            return serde_json::json!({"diagnostics": [], "note": "no language server running"});
        };
        let mut diagnostics: Vec<serde_json::Value> = service
            .diagnostics()
            .all_diagnostics()
            .into_iter()
            .filter_map(|(uri, items)| Some((uri_to_path(&uri)?, items)))
            .filter(|(path, _)| filter.as_ref().is_none_or(|wanted| wanted == path))
            .flat_map(|(path, items)| {
                items.into_iter().map(move |diag| {
                    serde_json::json!({
                        "path": path,
                        "line": diag.range.start.line + 1,
                        "column": diag.range.start.character + 1,
                        "end_line": diag.range.end.line + 1,
                        "end_column": diag.range.end.character + 1,
                        "severity": match diag.severity {
                            Some(lsp_types::DiagnosticSeverity::WARNING) => "warning",
                            Some(lsp_types::DiagnosticSeverity::INFORMATION) => "information",
                            Some(lsp_types::DiagnosticSeverity::HINT) => "hint",
                            _ => "error",
                        },
                        "source": diag.source,
                        "code": diag.code.as_ref().map(|code| match code {
                            lsp_types::NumberOrString::Number(n) => n.to_string(),
                            lsp_types::NumberOrString::String(s) => s.clone(),
                        }),
                        "message": diag.message,
                    })
                })
            })
            .take(2000)
            .collect();
        diagnostics.sort_by(|a, b| {
            a["path"]
                .as_str()
                .cmp(&b["path"].as_str())
                .then(a["line"].as_u64().cmp(&b["line"].as_u64()))
        });
        serde_json::json!({"diagnostics": diagnostics})
    }
}
