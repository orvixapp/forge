//! Real integration test against `rust-analyzer` (if installed in PATH).

use forge_buffer::Edit;
use forge_lsp::diagnostics::DiagnosticStore;
use forge_lsp::process::ServerInstance;
use forge_lsp::registry::ServerConfig;
use forge_lsp::sync::path_to_uri;
use lsp_types::{
    DocumentFormattingParams, FormattingOptions, HoverParams, Position, TextDocumentIdentifier,
    TextDocumentPositionParams, WorkDoneProgressParams,
};
use ropey::Rope;
use std::time::Duration;

#[tokio::test]
async fn test_real_rust_analyzer_lifecycle_and_sync() {
    // Check if rust-analyzer is available on the system
    let Ok(output) = std::process::Command::new("rust-analyzer")
        .arg("--version")
        .output()
    else {
        eprintln!("rust-analyzer not installed in PATH, skipping real server test");
        return;
    };
    if !output.status.success() {
        eprintln!("rust-analyzer --version failed, skipping real server test");
        return;
    }

    let temp_dir = tempfile::tempdir().unwrap();
    let root = temp_dir.path();
    let src_dir = root.join("src");
    std::fs::create_dir_all(&src_dir).unwrap();

    let cargo_toml = root.join("Cargo.toml");
    std::fs::write(
        &cargo_toml,
        "[package]\nname = \"ra_test\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();

    let main_rs = src_dir.join("main.rs");
    let initial_code =
        "fn greet() -> &'static str {\n    \"hello\"\n}\n\nfn main() {\n    let s = greet();\n}\n";
    std::fs::write(&main_rs, initial_code).unwrap();

    let config = ServerConfig::new("rust-analyzer", "rust-analyzer")
        .with_root_markers(vec!["Cargo.toml".to_string()]);

    let diag_store = DiagnosticStore::new();
    let mut instance = ServerInstance::new(config, root.to_path_buf(), diag_store.clone()).unwrap();

    // 1. Start rust-analyzer and perform handshake
    instance
        .start()
        .await
        .expect("Failed to start rust-analyzer");
    assert!(instance.server_capabilities().is_some());

    let client = instance.client().expect("Client missing after start");
    let file_uri = path_to_uri(&main_rs).expect("Valid URI");

    // 2. Open document (didOpen)
    let open_params = {
        let tracker_arc = instance.tracker();
        let mut tracker = tracker_arc.lock().await;
        tracker.did_open(
            file_uri.clone(),
            "rust".to_string(),
            initial_code.to_string(),
        )
    };
    client.did_open(open_params).await.expect("did_open failed");

    // Give rust-analyzer a moment to load the project
    tokio::time::sleep(Duration::from_millis(500)).await;

    // 3. Incremental change (didChange)
    let old_rope = Rope::from_str(initial_code);
    // Insert "fn calculate() -> i32 { 42 }\n\n" at the beginning
    let edit = Edit::insert(0, "fn calculate() -> i32 { 42 }\n\n");
    let new_text = format!("fn calculate() -> i32 {{ 42 }}\n\n{initial_code}");

    let change_params = {
        let tracker_arc = instance.tracker();
        let mut tracker = tracker_arc.lock().await;
        tracker.did_change_incremental(&file_uri, &[edit], &old_rope, new_text)
    }
    .expect("Valid change params");

    client
        .did_change(change_params)
        .await
        .expect("did_change failed");

    // 4. Test hover on "calculate" (line 0, character 4)
    let hover = wait_for_hover(client, &file_uri).await;
    assert!(serde_json::to_string(&hover).unwrap().contains("calculate"));
    assert_eq!(
        std::fs::read_to_string(&main_rs).unwrap(),
        initial_code,
        "LSP must see the unsaved buffer, not stale disk content"
    );

    // 5. Test formatting request
    let fmt_res = client
        .formatting(DocumentFormattingParams {
            text_document: TextDocumentIdentifier {
                uri: file_uri.clone(),
            },
            options: FormattingOptions {
                tab_size: 4,
                insert_spaces: true,
                ..Default::default()
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
        })
        .await;

    assert!(fmt_res.is_ok());

    // 6. Graceful shutdown
    instance.stop().await.expect("Shutdown failed");
    // Reopening an idle/crashed server must replay the unsaved tracked snapshot.
    instance.start().await.expect("Restart failed");
    let hover = wait_for_hover(instance.client().unwrap(), &file_uri).await;
    assert!(serde_json::to_string(&hover).unwrap().contains("calculate"));
    instance.stop().await.expect("Second shutdown failed");
}

async fn wait_for_hover(
    client: &forge_lsp::LspClient,
    file_uri: &lsp_types::Uri,
) -> lsp_types::Hover {
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let hover_res = client
                .hover(HoverParams {
                    text_document_position_params: TextDocumentPositionParams {
                        text_document: TextDocumentIdentifier {
                            uri: file_uri.clone(),
                        },
                        position: Position {
                            line: 0,
                            character: 4,
                        },
                    },
                    work_done_progress_params: WorkDoneProgressParams::default(),
                })
                .await;

            if let Ok(Some(hover)) = hover_res {
                return hover;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("rust-analyzer never returned hover for the unsaved calculate function")
}
