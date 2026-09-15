//! One asynchronous language service per window. GPUI only exchanges Rope
//! snapshots and bounded results; no process, RPC or diagnostic indexing on UI.

#[cfg(test)]
#[path = "lsp_tests.rs"]
mod tests;

use crate::assist::{Completion, CompletionItem, Diagnostic};
use crate::window::{ForgeWindow, NotificationLevel, Picker, PickerKind};
use forge_buffer::Edit;
use forge_gui::config::Config;
use forge_gui::i18n::tr;
use forge_lsp::{
    LanguageRegistry, LspManager, offset_to_position, path_to_uri, position_to_offset, uri_to_path,
};
use gpui::Context;
use ropey::Rope;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Feature {
    Completion,
    Hover,
    Definition,
    Signature,
    Problems,
}

#[derive(Clone)]
struct Snapshot {
    path: PathBuf,
    rope: Rope,
    version: u64,
    cursor: usize,
    dirty: bool,
    first_line: usize,
    last_line: usize,
}

#[derive(Clone)]
struct Request {
    feature: Feature,
    snapshot: Snapshot,
}

#[derive(Default)]
struct State {
    documents: HashMap<u64, Snapshot>,
    requests: HashMap<(u64, Feature), Request>,
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
        state
            .requests
            .insert((self.tab, feature), Request { feature, snapshot });
    }
}

pub struct LspService {
    state: Arc<Mutex<State>>,
    results: mpsc::Receiver<Update>,
    configuration: (
        forge_gui::config::LspConfig,
        Vec<forge_lsp::LanguageDefinition>,
    ),
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

enum Response {
    Completion(Completion),
    Info(String),
    Definition(lsp_types::GotoDefinitionResponse),
    Problems(Vec<Problem>),
}
enum Update {
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

    fn new(config: &Config) -> Self {
        let configuration = (config.lsp.clone(), config.languages.clone());
        let state = Arc::new(Mutex::new(State::default()));
        let (tx, results) = mpsc::sync_channel(128);
        let worker_state = state.clone();
        let worker_config = configuration.clone();
        std::thread::Builder::new()
            .name("forge-lsp".into())
            .spawn(move || {
                if let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    runtime.block_on(run(worker_state, tx, worker_config));
                }
            })
            .expect("spawn language service");
        Self {
            state,
            results,
            configuration,
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

fn same_position(a: &Snapshot, b: &Snapshot) -> bool {
    a.path == b.path && a.version == b.version && a.cursor == b.cursor
}

fn cancel_stale_requests(
    tasks: &mut HashMap<(u64, Feature), ActiveRequest>,
    snapshots: &HashMap<u64, Snapshot>,
) {
    tasks.retain(|(id, _), request| {
        if !snapshots
            .get(id)
            .is_some_and(|snapshot| same_position(snapshot, &request.snapshot))
        {
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

/// A background-only minimal replacement; coalesces every mutation path,
/// including multi-cursor, undo/redo, proposals and reloads without disk reads.
fn snapshot_delta(old: &Rope, new: &Rope) -> Vec<Edit> {
    let prefix = old
        .chars()
        .zip(new.chars())
        .take_while(|(a, b)| a == b)
        .count();
    let suffix = old
        .chars()
        .reversed()
        .zip(new.chars().reversed())
        .take(old.len_chars().min(new.len_chars()).saturating_sub(prefix))
        .take_while(|(a, b)| a == b)
        .count();
    vec![Edit {
        range: prefix..old.len_chars() - suffix,
        text: new.slice(prefix..new.len_chars() - suffix).to_string(),
    }]
}

fn language_manager(
    config: &forge_gui::config::LspConfig,
    languages: &[forge_lsp::LanguageDefinition],
) -> Arc<LspManager> {
    let mut registry = LanguageRegistry::new();
    for language in languages {
        registry.register(language.clone());
    }
    let mut manager = LspManager::new(registry, forge_lsp::DiagnosticStore::new());
    manager.set_idle_timeout(Duration::from_secs(config.idle_shutdown_secs.max(1)));
    Arc::new(manager)
}

async fn run(
    state: Arc<Mutex<State>>,
    results: mpsc::SyncSender<Update>,
    configuration: (
        forge_gui::config::LspConfig,
        Vec<forge_lsp::LanguageDefinition>,
    ),
) {
    let manager = language_manager(&configuration.0, &configuration.1);
    let mut documents: HashMap<u64, OpenDocument> = HashMap::new();
    let mut tasks: HashMap<(u64, Feature), ActiveRequest> = HashMap::new();
    let mut ticker = tokio::time::interval(Duration::from_millis(25));
    let mut maintenance = Instant::now();
    let mut published = HashMap::new();
    loop {
        ticker.tick().await;
        let (snapshots, requests, watched, shutdown) = {
            let mut state = state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (
                state.documents.clone(),
                std::mem::take(&mut state.requests),
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
            if !snapshots
                .get(&id)
                .is_some_and(|snapshot| same_position(snapshot, &request.snapshot))
            {
                continue;
            }
            if let Some(task) = tasks.remove(&(id, feature)) {
                task.task.abort();
            }
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
            let manager = manager.clone();
            let results = results.clone();
            let snapshot = request.snapshot.clone();
            let task = tokio::spawn(async move {
                if let Ok(Some(response)) = respond(&manager, &request).await {
                    let _ = results.try_send(Update::Response {
                        tab: id,
                        path: request.snapshot.path,
                        version: request.snapshot.version,
                        cursor: request.snapshot.cursor,
                        response,
                    });
                }
            });
            tasks.insert((id, feature), ActiveRequest { snapshot, task });
        }
        publish_diagnostics(&manager, &snapshots, &results, &mut published);
        if maintenance.elapsed() >= Duration::from_secs(1) {
            manager.maintenance().await;
            maintenance = Instant::now();
        }
    }
    for task in tasks.into_values() {
        task.task.abort();
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
        if doc.changed.elapsed() >= Duration::from_millis(wait) && Instant::now() >= doc.retry {
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

async fn respond(
    manager: &LspManager,
    request: &Request,
) -> Result<Option<Response>, forge_lsp::LspError> {
    let snapshot = &request.snapshot;
    let position = offset_to_position(&snapshot.rope, snapshot.cursor);
    Ok(match request.feature {
        Feature::Completion => {
            let items = manager.completion(&snapshot.path, position, None).await?;
            let start = prefix_start(&snapshot.rope, snapshot.cursor);
            let items: Vec<_> = items
                .iter()
                .filter_map(|item| completion_item(item, &snapshot.rope, start, snapshot.cursor))
                .collect();
            (!items.is_empty()).then(|| {
                Response::Completion(Completion {
                    items,
                    index: 0,
                    start,
                    prefix: snapshot.rope.slice(start..snapshot.cursor).to_string(),
                })
            })
        }
        Feature::Hover => manager
            .hover(&snapshot.path, position)
            .await?
            .map(|hover| Response::Info(hover_text(hover.contents))),
        Feature::Definition => manager
            .goto_definition(&snapshot.path, position)
            .await?
            .map(Response::Definition),
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
    })
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

fn completion_item(
    item: &lsp_types::CompletionItem,
    rope: &Rope,
    start: usize,
    cursor: usize,
) -> Option<CompletionItem> {
    if item.insert_text_format == Some(lsp_types::InsertTextFormat::SNIPPET) {
        return None;
    }
    let mut result = crate::assist::from_lsp_completion_item(item);
    let main = match &item.text_edit {
        Some(lsp_types::CompletionTextEdit::Edit(edit)) => Edit {
            range: forge_lsp::sync::lsp_range_to_range(rope, &edit.range),
            text: edit.new_text.clone(),
        },
        Some(lsp_types::CompletionTextEdit::InsertAndReplace(edit)) => Edit {
            range: forge_lsp::sync::lsp_range_to_range(rope, &edit.replace),
            text: edit.new_text.clone(),
        },
        None => Edit {
            range: start..cursor,
            text: item.insert_text.as_ref().unwrap_or(&item.label).clone(),
        },
    };
    let mut edits = vec![main];
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
    Some(result)
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
                state.documents.insert(
                    id,
                    Snapshot {
                        path: path.into(),
                        rope: editor.buffer.rope(),
                        version: editor.buffer.version(),
                        cursor: editor.buffer.selections().primary().head,
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
                    if !self.tabs[index].editor().is_some_and(|editor| {
                        editor.buffer.version() == version
                            && editor.path() == Some(path.as_path())
                            && editor.buffer.selections().primary().head == cursor
                    }) {
                        continue;
                    }
                    match response {
                        Response::Completion(completion) => {
                            if let Some(editor) = self.tabs[index].editor_mut() {
                                editor.completion = Some(completion);
                            }
                        }
                        Response::Info(text)
                            if self.active_tab().id == tab && self.picker.is_none() =>
                        {
                            self.picker = Some(Picker {
                                title: tr("Language server").into(),
                                items: vec![text],
                                index: 0,
                                kind: PickerKind::LspInfo,
                            });
                        }
                        Response::Definition(definition) if self.active_tab().id == tab => {
                            self.lsp_open_definition(definition);
                        }
                        Response::Problems(problems) if self.active_tab().id == tab => {
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
                        _ => {}
                    }
                    changed = true;
                }
                Update::Status(message) => self.notify_user(NotificationLevel::Warning, message),
            }
        }
        changed
    }

    pub(crate) fn lsp_feature(&mut self, feature: Feature, cx: &mut Context<Self>) {
        self.poll_lsp();
        if let Some(editor) = self.active_tab().editor()
            && let Some(handle) = &editor.lsp_handle
        {
            handle.request(
                feature,
                editor.buffer.version(),
                editor.buffer.selections().primary().head,
                editor.buffer.rope(),
            );
        }
        cx.notify();
    }

    fn lsp_open_definition(&mut self, definition: lsp_types::GotoDefinitionResponse) {
        let location = match definition {
            lsp_types::GotoDefinitionResponse::Scalar(location) => {
                Some((location.uri, location.range.start))
            }
            lsp_types::GotoDefinitionResponse::Array(items) => items
                .into_iter()
                .next()
                .map(|location| (location.uri, location.range.start)),
            lsp_types::GotoDefinitionResponse::Link(items) => items
                .into_iter()
                .next()
                .map(|location| (location.target_uri, location.target_selection_range.start)),
        };
        if let Some((uri, position)) = location
            && let Some(path) = uri_to_path(&uri)
        {
            self.lsp_navigation = Some((path, position));
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

    pub(crate) fn lsp_finish_navigation(&mut self, cx: &mut Context<Self>) {
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
}
