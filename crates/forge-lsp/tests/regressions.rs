//! Protocol regressions discovered while auditing the initial LSP spike.
use forge_buffer::Edit;
use forge_lsp::{DiagnosticStore, DocumentTracker, LspClient, LspError, Message, MockLspServer};
use lsp_types::{Diagnostic, Position, Range, Uri};
use ropey::Rope;
use std::str::FromStr;
use std::time::Duration;
use tokio::io::{AsyncWriteExt, BufReader, duplex};

#[test]
fn utf16_never_splits_surrogates_or_crosses_crlf() {
    let rope = Rope::from_str("a😀z\r\nnext\n");
    assert_eq!(forge_lsp::position_to_offset(&rope, Position::new(0, 2)), 1);
    assert_eq!(forge_lsp::position_to_offset(&rope, Position::new(0, 3)), 2);
    assert_eq!(
        forge_lsp::position_to_offset(&rope, Position::new(0, 100)),
        3
    );
    assert_eq!(forge_lsp::position_to_offset(&rope, Position::new(1, 0)), 5);
}

#[test]
fn overriding_language_removes_old_extensions() {
    let mut registry = forge_lsp::LanguageRegistry::new();
    let mut rust = registry.get_language_by_id("rust").unwrap().clone();
    rust.extensions = vec!["custom".into()];
    registry.register(rust);
    assert!(
        registry
            .detect_language(std::path::Path::new("main.rs"))
            .is_none()
    );
    assert_eq!(
        registry
            .detect_language(std::path::Path::new("main.custom"))
            .unwrap()
            .id,
        "rust"
    );
}

#[test]
fn exhausted_document_versions_do_not_wrap_or_mutate_text() {
    let uri = Uri::from_str("file:///workspace/main.rs").unwrap();
    let mut tracker = DocumentTracker::new();
    tracker.did_open_versioned(uri.clone(), "rust".into(), "original".into(), i32::MAX);
    assert!(tracker.did_change_full(&uri, "changed".into()).is_none());
    assert!(
        tracker
            .did_change_incremental(
                &uri,
                &[Edit::insert(0, "x")],
                &Rope::from_str("original"),
                "xoriginal".into()
            )
            .is_none()
    );
    assert_eq!(tracker.get(&uri).unwrap().text, "original");
    assert_eq!(tracker.version(&uri), Some(i32::MAX));
}

#[tokio::test]
async fn incremental_mock_uses_utf16_and_real_buffer_revisions() {
    let (client_io, server_io) = duplex(4096);
    let (reader, writer) = tokio::io::split(client_io);
    let (server_reader, server_writer) = tokio::io::split(server_io);
    let mock = MockLspServer::new();
    let documents = mock.documents();
    let server = tokio::spawn(mock.run(server_reader, server_writer, None));
    let client = LspClient::new(reader, writer);
    let uri = Uri::from_str("file:///workspace/main.rs").unwrap();
    let mut tracker = DocumentTracker::new();
    client
        .did_open(tracker.did_open_versioned(uri.clone(), "rust".into(), "a😀b\n".into(), 42))
        .await
        .unwrap();
    let mut changes = tracker
        .did_change_incremental(
            &uri,
            &[
                Edit {
                    range: 1..2,
                    text: "字".into(),
                },
                Edit::insert(3, "!"),
            ],
            &Rope::from_str("a😀b\n"),
            "a字b!\n".into(),
        )
        .unwrap();
    tracker.set_version(&uri, 57);
    changes.text_document.version = 57;
    assert_eq!(tracker.get(&uri).unwrap().version, 57);
    client.did_change(changes).await.unwrap();
    // A request is a protocol barrier: all earlier notifications were processed.
    client.send_request_raw("unknown", None).await.unwrap_err();
    assert_eq!(documents.lock().await.get(&uri).unwrap(), "a字b!\n");
    client
        .did_close(tracker.did_close(&uri).unwrap())
        .await
        .unwrap();
    client.send_request_raw("unknown", None).await.unwrap_err();
    assert!(!documents.lock().await.contains_key(&uri));
    server.abort();
}

#[tokio::test]
async fn aborting_a_request_sends_cancellation() {
    let (client_io, server_io) = duplex(4096);
    let (reader, writer) = tokio::io::split(client_io);
    let (server_reader, _server_writer) = tokio::io::split(server_io);
    let mut server_reader = BufReader::new(server_reader);
    let client = LspClient::new(reader, writer);
    let request_client = client.clone();
    let task = tokio::spawn(async move { request_client.send_request_raw("slow", None).await });
    let request = Message::parse(
        &forge_lsp::transport::read_message(&mut server_reader)
            .await
            .unwrap(),
    )
    .unwrap();
    let Message::Request(request) = request else {
        panic!("expected request")
    };
    task.abort();
    let _ = task.await;
    let payload = tokio::time::timeout(
        Duration::from_secs(1),
        forge_lsp::transport::read_message(&mut server_reader),
    )
    .await
    .unwrap()
    .unwrap();
    let Message::Notification(cancel) = Message::parse(&payload).unwrap() else {
        panic!("expected cancellation")
    };
    assert_eq!(cancel.method, "$/cancelRequest");
    assert_eq!(
        cancel.params.unwrap()["id"],
        serde_json::to_value(request.id).unwrap()
    );
}

#[tokio::test]
async fn timeout_includes_a_saturated_outgoing_queue() {
    let (client_io, _server_io) = duplex(1);
    let (reader, writer) = tokio::io::split(client_io);
    let client = LspClient::new(reader, writer);
    for _ in 0..129 {
        client.send_notification("queued", None).await.unwrap();
    }
    let response = tokio::time::timeout(
        Duration::from_secs(1),
        client.send_request_timeout("slow", None, Duration::from_millis(10)),
    )
    .await
    .unwrap();
    assert!(matches!(response, Err(LspError::Timeout)));
}

#[tokio::test]
async fn oversized_or_duplicate_headers_are_rejected_before_payload() {
    for raw in [
        "Content-Length: 16777217\r\n\r\n",
        "Content-Length: 1\r\nContent-Length: 1\r\n\r\n",
    ] {
        let (client, mut server) = duplex(4096);
        server.write_all(raw.as_bytes()).await.unwrap();
        assert!(matches!(
            forge_lsp::transport::read_message(&mut BufReader::new(client)).await,
            Err(forge_lsp::TransportError::HeaderFormat(_))
        ));
    }
}

#[tokio::test]
async fn publishing_diagnostics_does_not_discard_a_partial_request_frame() {
    let (client_io, server_io) = duplex(4096);
    let (reader, mut writer) = tokio::io::split(client_io);
    let (server_reader, server_writer) = tokio::io::split(server_io);
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    let task = tokio::spawn(MockLspServer::new().run(server_reader, server_writer, Some(rx)));
    let payload = br#"{"jsonrpc":"2.0","id":77,"method":"unknown"}"#;
    writer
        .write_all(format!("Content-Length: {}\r\n\r\n", payload.len()).as_bytes())
        .await
        .unwrap();
    writer.write_all(&payload[..10]).await.unwrap();
    tx.send(lsp_types::PublishDiagnosticsParams {
        uri: Uri::from_str("file:///workspace/main.rs").unwrap(),
        diagnostics: Vec::new(),
        version: Some(1),
    })
    .await
    .unwrap();
    let mut reader = BufReader::new(reader);
    let notification = forge_lsp::transport::read_message(&mut reader)
        .await
        .unwrap();
    assert!(matches!(
        Message::parse(&notification).unwrap(),
        Message::Notification(_)
    ));
    writer.write_all(&payload[10..]).await.unwrap();
    let response = tokio::time::timeout(
        Duration::from_secs(1),
        forge_lsp::transport::read_message(&mut reader),
    )
    .await
    .unwrap()
    .unwrap();
    let Message::Response(response) = Message::parse(&response).unwrap() else {
        panic!("expected response")
    };
    assert_eq!(response.id, Some(forge_lsp::Id::Number(77)));
    task.abort();
}

#[tokio::test]
async fn workspace_configuration_response_matches_requested_items() {
    let (client_io, server_io) = duplex(4096);
    let (reader, writer) = tokio::io::split(client_io);
    let (server_reader, mut server_writer) = tokio::io::split(server_io);
    let _client = LspClient::new(reader, writer);
    forge_lsp::transport::write_message(&mut server_writer, br#"{"jsonrpc":"2.0","id":"cfg","method":"workspace/configuration","params":{"items":[{"section":"a"},{"section":"b"}]}}"#).await.unwrap();
    let response = forge_lsp::transport::read_message(&mut BufReader::new(server_reader))
        .await
        .unwrap();
    let Message::Response(response) = Message::parse(&response).unwrap() else {
        panic!("expected response")
    };
    assert_eq!(response.result.unwrap(), serde_json::json!([null, null]));
}

#[test]
fn stale_diagnostics_do_not_replace_newer_results_and_huge_ranges_are_bounded() {
    let uri = Uri::from_str("file:///workspace/main.rs").unwrap();
    let store = DiagnosticStore::new();
    store.update(
        uri.clone(),
        Some(57),
        vec![Diagnostic {
            range: Range::new(Position::new(0, 0), Position::new(u32::MAX, 1)),
            message: "new".into(),
            ..Default::default()
        }],
    );
    store.update(uri.clone(), Some(42), Vec::new());
    assert_eq!(store.version_for_document(&uri), Some(57));
    assert_eq!(store.for_line(&uri, 500).len(), 1);
    assert_eq!(store.counts_for_document(&uri), (1, 0, 1));
}
