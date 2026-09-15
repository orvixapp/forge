//! Language Server Protocol implementation for Forge (ARCHITECTURE.md §15).
//!
//! Provides a custom client over `lsp-types` with `Content-Length` framing over
//! stdio / async I/O, lifecycle management, incremental document synchronization
//! with UTF-16 coordinate mapping, high-capacity diagnostics store, and multi-server
//! capability orchestration.

pub mod capabilities;
pub mod client;
pub mod diagnostics;
pub mod jsonrpc;
pub mod manager;
pub mod mock;
pub mod process;
pub mod registry;
pub mod sync;
pub mod transport;
pub mod workspace_edit;

pub use capabilities::client_capabilities;
pub use client::{LspClient, LspError};
pub use diagnostics::{DiagnosticStore, DocumentDiagnostics};
pub use jsonrpc::{Id, Message, Notification, Request, Response, ResponseError};
pub use manager::{Indentation, LspManager, ServerKey};
pub use mock::MockLspServer;
pub use process::{ServerInstance, ServerStatus};
pub use registry::{LanguageDefinition, LanguageRegistry, ServerConfig};
pub use sync::{
    DocumentTracker, offset_to_position, path_to_uri, position_to_offset, snapshot_delta,
    uri_to_path,
};
pub use transport::TransportError;

#[cfg(test)]
mod tests {
    use super::*;
    use lsp_types::*;
    use std::str::FromStr;
    use tokio::io::duplex;

    #[tokio::test]
    // Keep the handshake-to-shutdown acceptance sequence in one test.
    #[allow(clippy::too_many_lines)]
    async fn test_full_mock_server_interaction() {
        let (client_io, server_io) = duplex(4096);
        let (server_reader, server_writer) = tokio::io::split(server_io);
        let (client_reader, client_writer) = tokio::io::split(client_io);

        let (diag_tx, diag_rx) = tokio::sync::mpsc::channel(16);
        let mock_server = MockLspServer::new();
        let docs = mock_server.documents();
        let cancelled = mock_server.cancelled_requests();

        // Spawn mock server
        tokio::spawn(async move {
            mock_server
                .run(server_reader, server_writer, Some(diag_rx))
                .await;
        });

        // Initialize client
        let client = LspClient::new(client_reader, client_writer);
        let mut notif_rx = client.subscribe_notifications();

        #[allow(deprecated)]
        let init_params = InitializeParams {
            process_id: Some(1234),
            root_path: None,
            root_uri: Some(Uri::from_str("file:///workspace").unwrap()),
            initialization_options: None,
            capabilities: client_capabilities(),
            trace: None,
            workspace_folders: None,
            client_info: None,
            locale: None,
            work_done_progress_params: WorkDoneProgressParams::default(),
        };

        // 1. Handshake
        let init_res = client.initialize(init_params).await.unwrap();
        assert_eq!(
            init_res.server_info.as_ref().map(|s| s.name.as_str()),
            Some("mock-lsp")
        );
        client.initialized().await.unwrap();

        // 2. Open document
        let doc_uri = Uri::from_str("file:///workspace/src/main.rs").unwrap();
        client
            .did_open(DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: doc_uri.clone(),
                    language_id: "rust".to_string(),
                    version: 1,
                    text: "fn main() {\n    test_\n}".to_string(),
                },
            })
            .await
            .unwrap();

        // Give the mock server a tick to process did_open
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(docs.lock().await.contains_key(&doc_uri));

        // 3. Completion
        let comp_res = client
            .completion(CompletionParams {
                text_document_position: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier {
                        uri: doc_uri.clone(),
                    },
                    position: Position {
                        line: 1,
                        character: 9,
                    },
                },
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
                context: None,
            })
            .await
            .unwrap()
            .unwrap();

        let items = match comp_res {
            CompletionResponse::Array(arr) => arr,
            CompletionResponse::List(l) => l.items,
        };
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].label, "test_fn");

        // 4. Completion resolve
        let resolved = client
            .resolve_completion_item(items[0].clone())
            .await
            .unwrap();
        assert!(resolved.documentation.is_some());

        // 5. Hover
        let hover = client
            .hover(HoverParams {
                text_document_position_params: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier {
                        uri: doc_uri.clone(),
                    },
                    position: Position {
                        line: 1,
                        character: 6,
                    },
                },
                work_done_progress_params: WorkDoneProgressParams::default(),
            })
            .await
            .unwrap()
            .unwrap();

        match hover.contents {
            HoverContents::Markup(m) => assert!(m.value.contains("Mock Hover")),
            _ => panic!("Expected markdown hover"),
        }

        // 6. Push diagnostics from server
        diag_tx
            .send(PublishDiagnosticsParams {
                uri: doc_uri.clone(),
                diagnostics: vec![Diagnostic {
                    range: Range {
                        start: Position {
                            line: 1,
                            character: 4,
                        },
                        end: Position {
                            line: 1,
                            character: 9,
                        },
                    },
                    severity: Some(DiagnosticSeverity::ERROR),
                    message: "cannot find function `test_` in this scope".to_string(),
                    ..Default::default()
                }],
                version: Some(1),
            })
            .await
            .unwrap();

        let notif = notif_rx.recv().await.unwrap();
        assert_eq!(notif.method, "textDocument/publishDiagnostics");

        // 7. Cancellation test
        client.cancel_request(Id::Number(99)).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert_eq!(*cancelled.lock().await, vec![Id::Number(99)]);

        // 8. Shutdown and exit
        client.shutdown().await.unwrap();
        client.exit().await.unwrap();
    }
}
