//! Acceptance of the editor's first vertical against a real `rust-analyzer`
//! through `LspManager`, the same API `forge-gui` drives: versioned didOpen,
//! typing, undo, pull diagnostics, hover, completion, definition, references,
//! rename, code actions, formatting and workspace symbols. Nothing here
//! touches the file on disk after the initial write. Skipped when
//! `rust-analyzer` is not in `PATH`.

use forge_lsp::{DiagnosticStore, Indentation, LanguageRegistry, LspManager, snapshot_delta};
use lsp_types::{CodeActionOrCommand, GotoDefinitionResponse, Position};
use ropey::Rope;
use std::path::Path;
use std::time::{Duration, Instant};

const ORIGINAL: &str = "fn greet(name: &str) -> String {\n    format!(\"hello {name}\")\n}\n\nfn main(){\n    let message = greet(\"forge\");\n    println!(\"{message}\");\n}\n";

struct Document {
    path: std::path::PathBuf,
    rope: Rope,
    version: u64,
    history: Vec<Rope>,
}

impl Document {
    /// Applies a change the way the editor's worker does: one coalesced
    /// delta between the previous and the current snapshot.
    async fn change(&mut self, manager: &LspManager, new_text: &str) {
        let new_rope = Rope::from_str(new_text);
        self.history.push(self.rope.clone());
        self.version += 1;
        manager
            .change_document_versioned(
                &self.path,
                &snapshot_delta(&self.rope, &new_rope),
                &self.rope,
                new_text.to_owned(),
                self.version,
            )
            .await
            .expect("didChange");
        self.rope = new_rope;
    }

    async fn undo(&mut self, manager: &LspManager) {
        let previous = self.history.pop().expect("history").to_string();
        self.change(manager, &previous).await;
        self.history.pop();
    }
}

fn at(line: u32, character: u32) -> Position {
    Position { line, character }
}

async fn wait_for<T>(what: &str, mut probe: impl AsyncFnMut() -> Option<T>) -> T {
    let started = Instant::now();
    loop {
        if let Some(value) = probe().await {
            return value;
        }
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "rust-analyzer never produced {what}"
        );
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

fn messages(manager: &LspManager, path: &Path) -> Vec<String> {
    let uri = forge_lsp::path_to_uri(path).unwrap();
    manager
        .diagnostics()
        .for_document(&uri)
        .into_iter()
        .map(|diagnostic| diagnostic.message)
        .collect()
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn editor_vertical_against_rust_analyzer() {
    if !std::process::Command::new("rust-analyzer")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        eprintln!("rust-analyzer not installed in PATH, skipping real server test");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"vertical\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    let path = root.join("src/main.rs");
    std::fs::write(&path, ORIGINAL).unwrap();

    // Native diagnostics only: `cargo check` would add seconds and disk I/O.
    let mut registry = LanguageRegistry::new();
    let mut rust = registry.get_language_by_id("rust").unwrap().clone();
    rust.servers[0].initialization_options = Some(serde_json::json!({"checkOnSave": false}));
    registry.register(rust);
    let manager = LspManager::new(registry, DiagnosticStore::new());
    let mut doc = Document {
        path: path.clone(),
        rope: Rope::from_str(ORIGINAL),
        version: 1,
        history: Vec::new(),
    };
    manager
        .open_document_versioned(&path, ORIGINAL.to_owned(), doc.version)
        .await
        .expect("didOpen");
    let uri = forge_lsp::path_to_uri(&path).unwrap();

    // Trigger characters are negotiated, not hard-coded.
    wait_for("capabilities", async || {
        manager.completion_trigger(&path, '.').await.then_some(())
    })
    .await;
    assert!(manager.completion_trigger(&path, ':').await);
    assert!(!manager.completion_trigger(&path, 'x').await);

    // 1. Typing an error without saving: pull diagnostics at the exact revision.
    let with_error = ORIGINAL.replace(
        "    println!(\"{message}\");\n",
        "    println!(\"{message}\");\n    let n: u32 = \"text\";\n",
    );
    doc.change(&manager, &with_error).await;
    wait_for("the type mismatch", async || {
        manager.pull_diagnostics(&path, doc.version).await.ok()?;
        messages(&manager, &path)
            .iter()
            .any(|message| message.contains("expected u32"))
            .then_some(())
    })
    .await;
    assert_eq!(manager.diagnostics().version_for_document(&uri), Some(2));
    assert_eq!(manager.diagnostics().for_line(&uri, 7).len(), 1);

    // 2. Hover on the call site sees the unsaved buffer.
    let hover = wait_for("hover", async || {
        manager.hover(&path, at(5, 18)).await.ok().flatten()
    })
    .await;
    assert!(serde_json::to_string(&hover).unwrap().contains("greet"));

    // 3. Completion while typing `gre`: the server answers with candidates the
    // editor must filter; function calls arrive as snippets.
    let typing = with_error.replace(
        "    let n: u32 = \"text\";\n",
        "    let n: u32 = \"text\";\n    gre\n",
    );
    doc.change(&manager, &typing).await;
    let items = wait_for("completion", async || {
        let items = manager.completion(&path, at(8, 7), None).await.ok()?;
        items
            .iter()
            .any(|item| item.label.starts_with("greet"))
            .then_some(items)
    })
    .await;
    let greet = items
        .iter()
        .find(|item| item.label.starts_with("greet"))
        .unwrap();
    assert_eq!(
        greet.insert_text_format,
        Some(lsp_types::InsertTextFormat::SNIPPET)
    );
    let inserted = match &greet.text_edit {
        Some(lsp_types::CompletionTextEdit::Edit(edit)) => edit.new_text.clone(),
        Some(lsp_types::CompletionTextEdit::InsertAndReplace(edit)) => edit.new_text.clone(),
        None => greet.insert_text.clone().unwrap_or_default(),
    };
    assert!(
        inserted.contains("${1:"),
        "snippet placeholder in {inserted}"
    );
    assert!(
        items.iter().any(|item| !item.label.starts_with("gre")),
        "the server does not filter by prefix; the editor must"
    );
    let resolved = manager
        .resolve_completion(&path, greet.clone())
        .await
        .expect("completionItem/resolve");
    assert_eq!(resolved.label, greet.label);

    // 4. Definition of the call goes to the declaration.
    let definition = manager
        .goto_definition(&path, at(5, 18))
        .await
        .expect("definition")
        .expect("a location");
    let line = match definition {
        GotoDefinitionResponse::Scalar(location) => location.range.start.line,
        GotoDefinitionResponse::Array(locations) => locations[0].range.start.line,
        GotoDefinitionResponse::Link(links) => links[0].target_selection_range.start.line,
    };
    assert_eq!(line, 0);

    // 5. References include the declaration and the call.
    let references = manager
        .references(&path, at(5, 18), true)
        .await
        .expect("references");
    let mut lines: Vec<u32> = references
        .iter()
        .map(|location| location.range.start.line)
        .collect();
    lines.sort_unstable();
    assert_eq!(lines, [0, 5]);

    // 6. Undo twice: revisions keep growing, diagnostics follow the text.
    doc.undo(&manager).await;
    doc.undo(&manager).await;
    assert_eq!(doc.rope.to_string(), ORIGINAL);
    assert_eq!(doc.version, 5);
    wait_for("clean diagnostics after undo", async || {
        manager.pull_diagnostics(&path, doc.version).await.ok()?;
        messages(&manager, &path).is_empty().then_some(())
    })
    .await;
    assert_eq!(manager.diagnostics().version_for_document(&uri), Some(5));

    // 7. Rename with prepare: two edits in this file.
    let prepared = manager
        .prepare_rename(&path, at(5, 18))
        .await
        .expect("prepareRename");
    assert!(prepared.is_some());
    let edit = manager
        .rename(&path, at(5, 18), "salute".into())
        .await
        .expect("rename")
        .expect("workspace edit");
    let count = forge_lsp::workspace_edit::edit_count(
        &forge_lsp::workspace_edit::text_edits(&edit).expect("text edits only"),
    );
    assert_eq!(count, 2);

    // 8. Code actions are listed lazily and resolved on demand (the server
    // may still be re-analysing right after the undo, hence the polling).
    let explicit_type = wait_for("the add explicit type assist", async || {
        let actions = manager
            .code_actions(
                &path,
                lsp_types::Range::new(at(5, 8), at(5, 15)),
                Vec::new(),
            )
            .await
            .ok()?;
        actions.into_iter().find_map(|action| match action {
            CodeActionOrCommand::CodeAction(action) if action.title.contains("explicit type") => {
                Some(action)
            }
            _ => None,
        })
    })
    .await;
    let resolved = manager
        .resolve_code_action(&path, explicit_type)
        .await
        .expect("codeAction/resolve");
    assert!(resolved.edit.is_some());

    // 9. Formatting fixes `fn main(){` without the editor writing anything.
    let edits = manager
        .formatting(&path, Indentation::default())
        .await
        .expect("formatting")
        .expect("edits");
    assert!(!edits.is_empty());

    // 10. Workspace symbols find the function.
    let symbols = wait_for("workspace symbols", async || {
        let symbols = manager.workspace_symbols("greet").await.ok()?;
        (!symbols.is_empty()).then_some(symbols)
    })
    .await;
    assert!(symbols.iter().any(|symbol| symbol.name == "greet"));

    // Saving announces the revision the server already has; disk untouched.
    manager
        .save_document(&path, Some(doc.rope.to_string()))
        .await
        .expect("didSave");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), ORIGINAL);
    manager.close_document(&path).await.expect("didClose");
    assert!(messages(&manager, &path).is_empty());
    manager.shutdown_all().await;
}
