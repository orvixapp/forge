// Included from lsp.rs to test the pure UI/worker boundary without GPUI.
use super::*;

#[test]
fn coalesced_snapshot_delta_preserves_unicode_and_multiple_changes() {
    for (before, after) in [
        ("a😀b\nold\n", "a字b!\nnew\n"),
        ("abc", "abc"),
        ("", "😀"),
        ("abc", ""),
    ] {
        let old = Rope::from_str(before);
        let new = Rope::from_str(after);
        let mut applied = old.clone();
        for edit in snapshot_delta(&old, &new) {
            applied.remove(edit.range.clone());
            applied.insert(edit.range.start, &edit.text);
        }
        assert_eq!(applied.to_string(), after);
    }
}

#[test]
fn completion_uses_server_replacement_and_additional_edits() {
    let rope = Rope::from_str("😀\nfoo\n");
    let item = lsp_types::CompletionItem {
        label: "display label".into(),
        text_edit: Some(lsp_types::CompletionTextEdit::Edit(lsp_types::TextEdit {
            range: lsp_types::Range::new(
                lsp_types::Position::new(1, 0),
                lsp_types::Position::new(1, 3),
            ),
            new_text: "actual_insert".into(),
        })),
        additional_text_edits: Some(vec![lsp_types::TextEdit {
            range: lsp_types::Range::new(
                lsp_types::Position::new(0, 0),
                lsp_types::Position::new(0, 0),
            ),
            new_text: "use module;\n".into(),
        }]),
        ..Default::default()
    };
    let result = completion_item(&item, &rope, 2, 5);
    let edits = result.edits.unwrap();
    assert_eq!(edits[0].range, 2..5);
    assert_eq!(edits[0].text, "actual_insert");
    assert_eq!(edits[1].range, 0..0);
}

#[test]
fn save_as_and_cursor_changes_invalidate_requests() {
    let mut snapshot = Snapshot {
        path: "/tmp/a.rs".into(),
        rope: Rope::new(),
        version: 2,
        cursor: 0,
        anchor: 0,
        dirty: true,
        first_line: 0,
        last_line: 10,
    };
    let original = snapshot.clone();
    snapshot.path = "/tmp/b.rs".into();
    assert!(!same_position(&original, &snapshot));
    snapshot = original.clone();
    snapshot.cursor = 1;
    assert!(!same_position(&original, &snapshot));
}

#[test]
fn completions_are_filtered_and_ranked_client_side() {
    let item = |label: &str, filter: Option<&str>, sort: Option<&str>| lsp_types::CompletionItem {
        label: label.into(),
        filter_text: filter.map(str::to_owned),
        sort_text: sort.map(str::to_owned),
        ..Default::default()
    };
    let items = vec![
        item("read_line", None, Some("b")),
        item("Reader", None, Some("a")),
        item("unrelated", None, Some("0")),
        item("rl_alias", Some("read_line"), Some("c")),
        item("rdl", None, None),
    ];
    let labels = |prefix: &str| -> Vec<String> {
        rank_completions(&items, prefix)
            .into_iter()
            .map(|item| item.label.clone())
            .collect()
    };
    assert_eq!(labels("re"), ["read_line", "rl_alias", "Reader"]);
    assert_eq!(labels("rdl"), ["rdl", "read_line", "rl_alias"]);
    assert_eq!(labels("RL"), ["read_line", "rl_alias", "rdl"]);
    assert_eq!(
        labels("ader"),
        Vec::<String>::new(),
        "mid-word starts do not match"
    );
    assert_eq!(labels("").len(), 5, "no prefix keeps the server's order");
    assert!(labels("zzz").is_empty());
}

#[test]
fn snippet_items_expand_and_keep_tabstops() {
    let rope = Rope::from_str("gre\n");
    let item = lsp_types::CompletionItem {
        label: "greet(…)".into(),
        insert_text: Some("greet(${1:name})$0".into()),
        insert_text_format: Some(lsp_types::InsertTextFormat::SNIPPET),
        data: Some(serde_json::json!({"id": 1})),
        ..Default::default()
    };
    let result = completion_item(&item, &rope, 0, 3);
    let edits = result.edits.unwrap();
    assert_eq!(edits[0].text, "greet(name)");
    let stop = &result.snippet.unwrap().tabstops[0];
    assert_eq!((stop.ranges.len(), stop.ranges[0].clone()), (1, 6..10));
    assert!(
        result.lsp.is_some(),
        "resolvable items keep the server item"
    );
    let plain = lsp_types::CompletionItem {
        label: "x".into(),
        insert_text: Some("$0".into()),
        insert_text_format: Some(lsp_types::InsertTextFormat::SNIPPET),
        ..Default::default()
    };
    let result = completion_item(&plain, &rope, 0, 3);
    assert!(
        result.snippet.is_none(),
        "a lone final cursor is not a session"
    );
    assert!(result.lsp.is_none());
}

/// The window's worker against a real rust-analyzer: open at a revision,
/// diagnostics arrive by pull for that revision, then completion (filtered
/// client-side, snippet expanded), hover, definition and prepareRename
/// through the same handle the editor uses. Skipped without rust-analyzer.
#[test]
// One scenario in one test: the steps depend on the same server session.
#[allow(clippy::too_many_lines)]
fn worker_drives_rust_analyzer_like_the_editor() {
    if !std::process::Command::new("rust-analyzer")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        eprintln!("rust-analyzer not installed in PATH, skipping worker test");
        return;
    }
    let root = std::env::temp_dir().join(format!("forge-lsp-worker-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"worker\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    let path = std::fs::canonicalize(&root).unwrap().join("src/main.rs");
    let text = "fn greet(name: &str) -> String {\n    format!(\"hello {name}\")\n}\n\nfn main() {\n    let n: u32 = greet(\"x\");\n}\n";
    std::fs::write(&path, text).unwrap();

    let mut config = Config::default();
    config.lsp.startup_ms = 0;
    config.lsp.debounce_ms = 10;
    let mut rust = LanguageRegistry::new()
        .get_language_by_id("rust")
        .unwrap()
        .clone();
    rust.servers[0].initialization_options = Some(serde_json::json!({"checkOnSave": false}));
    config.languages = vec![rust];
    let service = LspService::new(&config);
    let handle = LspHandle {
        tab: 7,
        state: service.state.clone(),
    };
    let snapshot = |rope: &Rope, version: u64, cursor: usize| Snapshot {
        path: path.clone(),
        rope: rope.clone(),
        version,
        cursor,
        anchor: cursor,
        dirty: version > 1,
        first_line: 0,
        last_line: 40,
    };
    let publish = |snapshot: Snapshot| {
        service.state.lock().unwrap().documents.insert(7, snapshot);
    };
    let wait = |what: &str, mut accept: Box<dyn FnMut(Update) -> bool>| {
        let started = Instant::now();
        loop {
            while let Ok(update) = service.results.try_recv() {
                if accept(update) {
                    return;
                }
            }
            assert!(
                started.elapsed() < Duration::from_secs(60),
                "the worker never produced {what}"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    };

    // 1. Open at revision 1: the type mismatch arrives by pull, versioned.
    let rope = Rope::from_str(text);
    publish(snapshot(&rope, 1, 0));
    wait(
        "pull diagnostics",
        Box::new(|update| {
            matches!(
                update,
                Update::Diagnostics { tab: 7, version: 1, ref diagnostics, counts: (1, 0, 1) }
                    if diagnostics[0].message.contains("expected u32")
            )
        }),
    );

    // 2. Type `gre` on a new line: completion is filtered and expanded.
    let typed = text.replace(
        "    let n: u32 = greet(\"x\");\n",
        "    let n: u32 = greet(\"x\");\n    gre\n",
    );
    let rope = Rope::from_str(&typed);
    let cursor = rope.line_to_char(6) + 7;
    publish(snapshot(&rope, 2, cursor));
    handle.request(Feature::Completion, 2, cursor, rope.clone());
    wait(
        "completion",
        Box::new(|update| match update {
            Update::Response {
                tab: 7,
                version: 2,
                response: Response::Completion(completion),
                ..
            } => {
                assert!(completion.items.iter().all(|item| {
                    let candidate = item.label.to_lowercase();
                    candidate.starts_with("gre") || candidate.contains('g')
                }));
                let greet = completion
                    .items
                    .iter()
                    .find(|item| item.label.starts_with("greet"))
                    .expect("greet is offered");
                assert!(greet.snippet.is_some(), "call snippet expanded");
                assert_eq!(greet.edits.as_ref().unwrap()[0].text, "greet(name)");
                assert_eq!(completion.prefix, "gre");
                true
            }
            _ => false,
        }),
    );

    // 3. Hover, definition and prepareRename on the call in `main`.
    let call = rope.line_to_char(5) + 18;
    publish(snapshot(&rope, 2, call));
    handle.request(Feature::Hover, 2, call, rope.clone());
    wait(
        "hover",
        Box::new(
            |update| matches!(update, Update::Response { response: Response::Info(text), .. } if text.contains("greet")),
        ),
    );
    handle.request(Feature::Definition, 2, call, rope.clone());
    wait(
        "definition",
        Box::new(|update| match update {
            Update::Response {
                response: Response::Locations(locations),
                ..
            } => {
                assert_eq!(locations.len(), 1);
                assert_eq!(locations[0].position.line, 0);
                assert!(locations[0].preview.starts_with("fn greet"));
                true
            }
            _ => false,
        }),
    );
    handle.request(Feature::PrepareRename, 2, call, rope.clone());
    wait(
        "prepareRename",
        Box::new(
            |update| matches!(update, Update::Response { response: Response::RenamePlaceholder(name), .. } if name == "greet"),
        ),
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
    drop(service);
    let _ = std::fs::remove_dir_all(&root);
}
