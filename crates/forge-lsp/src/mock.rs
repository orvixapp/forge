//! In-memory mock LSP server for integration testing of transport, protocol, sync and capabilities.

use crate::jsonrpc::{Id, Message, Notification, Request, Response};
use crate::transport::{read_message, write_message};
use lsp_types::{
    CancelParams, CompletionItem, CompletionItemKind, CompletionOptions, CompletionResponse,
    DidChangeTextDocumentParams, DidOpenTextDocumentParams, Documentation, GotoDefinitionResponse,
    Hover, HoverContents, HoverProviderCapability, InitializeResult, Location, MarkupContent,
    MarkupKind, NumberOrString, OneOf, Position, PublishDiagnosticsParams, Range,
    ServerCapabilities, ServerInfo, SymbolInformation, SymbolKind, TextDocumentSyncCapability,
    TextDocumentSyncKind, TextEdit, Uri,
};
use serde_json::json;
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite, BufReader};
use tokio::sync::{Mutex, mpsc};

pub struct MockLspServer {
    documents: Arc<Mutex<HashMap<Uri, String>>>,
    cancelled_requests: Arc<Mutex<Vec<Id>>>,
}

impl Default for MockLspServer {
    fn default() -> Self {
        Self::new()
    }
}

impl MockLspServer {
    #[must_use]
    pub fn new() -> Self {
        Self {
            documents: Arc::new(Mutex::new(HashMap::new())),
            cancelled_requests: Arc::new(Mutex::new(Vec::new())),
        }
    }

    #[must_use]
    pub fn documents(&self) -> Arc<Mutex<HashMap<Uri, String>>> {
        Arc::clone(&self.documents)
    }

    #[must_use]
    pub fn cancelled_requests(&self) -> Arc<Mutex<Vec<Id>>> {
        Arc::clone(&self.cancelled_requests)
    }

    /// Runs the mock server loop over an async duplex reader and writer.
    ///
    /// # Panics
    /// Panics only if serialization of the mock's fixed protocol fixtures fails.
    pub async fn run<R, W>(
        self,
        reader: R,
        mut writer: W,
        mut diagnostics_rx: Option<mpsc::Receiver<PublishDiagnosticsParams>>,
    ) where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let documents = Arc::clone(&self.documents);
        let cancelled = Arc::clone(&self.cancelled_requests);
        let (incoming_tx, mut incoming_rx) = mpsc::channel(16);
        let reader_task = tokio::spawn(async move {
            let mut buf_reader = BufReader::new(reader);
            loop {
                let message = read_message(&mut buf_reader).await;
                let closed = message.is_err();
                if incoming_tx.send(message).await.is_err() || closed {
                    break;
                }
            }
        });

        loop {
            tokio::select! {
                // Check for server-initiated diagnostics to push
                Some(params) = async {
                    match &mut diagnostics_rx {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending().await,
                    }
                } => {
                    let notif = Message::Notification(Notification {
                        jsonrpc: "2.0".to_string(),
                        method: "textDocument/publishDiagnostics".to_string(),
                        params: Some(serde_json::to_value(params).unwrap()),
                    });
                    let bytes = notif.to_bytes().unwrap();
                    let _ = write_message(&mut writer, &bytes).await;
                }

                // Incoming client messages
                Some(read_res) = incoming_rx.recv() => {
                    let Ok(payload) = read_res else {
                        break;
                    };
                    let Ok(msg) = Message::parse(&payload) else {
                        continue;
                    };

                    match msg {
                        Message::Request(req) => {
                            let resp = Self::handle_request(&req, &documents);
                            let bytes = Message::Response(resp).to_bytes().unwrap();
                            if write_message(&mut writer, &bytes).await.is_err() {
                                break;
                            }
                        }
                        Message::Notification(notif) => {
                            if notif.method == "$/cancelRequest" {
                                if let Some(params) = notif.params
                                    && let Ok(cancel) = serde_json::from_value::<CancelParams>(params) {
                                        let id = match cancel.id {
                                            NumberOrString::Number(n) => Id::Number(i64::from(n)),
                                            NumberOrString::String(s) => Id::String(s),
                                        };
                                        cancelled.lock().await.push(id);
                                    }
                            } else if notif.method == "textDocument/didOpen" {
                                if let Some(params) = notif.params
                                    && let Ok(open) = serde_json::from_value::<DidOpenTextDocumentParams>(params) {
                                        documents.lock().await.insert(open.text_document.uri, open.text_document.text);
                                    }
                            } else if notif.method == "textDocument/didChange" {
                                if let Some(params) = notif.params
                                    && let Ok(change) = serde_json::from_value::<DidChangeTextDocumentParams>(params) {
                                        let mut docs = documents.lock().await;
                                        if let Some(doc) = docs.get_mut(&change.text_document.uri) {
                                            for c in change.content_changes {
                                                if let Some(range) = c.range {
                                                    let mut rope = ropey::Rope::from_str(doc);
                                                    let chars = crate::sync::lsp_range_to_range(&rope, &range);
                                                    rope.remove(chars.clone());
                                                    rope.insert(chars.start, &c.text);
                                                    *doc = rope.to_string();
                                                } else { *doc = c.text; }
                                            }
                                        }
                                    }
                            } else if notif.method == "textDocument/didClose" {
                                if let Some(params) = notif.params
                                    && let Ok(close) = serde_json::from_value::<lsp_types::DidCloseTextDocumentParams>(params) {
                                    documents.lock().await.remove(&close.text_document.uri);
                                }
                            } else if notif.method == "exit" {
                                break;
                            }
                        }
                        Message::Response(_) => {}
                    }
                }
            }
        }
        reader_task.abort();
    }

    // A single fixture table keeps the mock protocol responses auditable.
    #[allow(clippy::too_many_lines)]
    fn handle_request(req: &Request, _docs: &Arc<Mutex<HashMap<Uri, String>>>) -> Response {
        let result = match req.method.as_str() {
            "initialize" => {
                #[allow(deprecated)]
                let caps = ServerCapabilities {
                    text_document_sync: Some(TextDocumentSyncCapability::Kind(
                        TextDocumentSyncKind::INCREMENTAL,
                    )),
                    completion_provider: Some(CompletionOptions {
                        resolve_provider: Some(true),
                        trigger_characters: Some(vec![".".to_string(), ":".to_string()]),
                        ..Default::default()
                    }),
                    hover_provider: Some(HoverProviderCapability::Simple(true)),
                    definition_provider: Some(OneOf::Left(true)),
                    references_provider: Some(OneOf::Left(true)),
                    document_formatting_provider: Some(OneOf::Left(true)),
                    workspace_symbol_provider: Some(OneOf::Left(true)),
                    ..Default::default()
                };
                json!(InitializeResult {
                    capabilities: caps,
                    server_info: Some(ServerInfo {
                        name: "mock-lsp".to_string(),
                        version: Some("1.0.0".to_string()),
                    }),
                })
            }
            "textDocument/completion" => {
                json!(CompletionResponse::Array(vec![
                    CompletionItem {
                        label: "test_fn".to_string(),
                        kind: Some(CompletionItemKind::FUNCTION),
                        detail: Some("fn test_fn() -> i32".to_string()),
                        documentation: Some(Documentation::String("A mock function".to_string())),
                        insert_text: Some("test_fn()".to_string()),
                        ..Default::default()
                    },
                    CompletionItem {
                        label: "test_var".to_string(),
                        kind: Some(CompletionItemKind::VARIABLE),
                        detail: Some("let test_var: bool".to_string()),
                        ..Default::default()
                    }
                ]))
            }
            "completionItem/resolve" => {
                let mut item: CompletionItem =
                    serde_json::from_value(req.params.clone().unwrap()).unwrap();
                item.documentation = Some(Documentation::String(
                    "Resolved documentation from server".to_string(),
                ));
                json!(item)
            }
            "textDocument/hover" => {
                json!(Hover {
                    contents: HoverContents::Markup(MarkupContent {
                        kind: MarkupKind::Markdown,
                        value: "# Mock Hover\nThis is a mock hover description.".to_string(),
                    }),
                    range: None,
                })
            }
            "textDocument/definition" => {
                json!(GotoDefinitionResponse::Scalar(Location {
                    uri: Uri::from_str("file:///src/main.rs").unwrap(),
                    range: Range {
                        start: Position {
                            line: 10,
                            character: 4
                        },
                        end: Position {
                            line: 10,
                            character: 11
                        },
                    },
                }))
            }
            "textDocument/formatting" => {
                json!(vec![TextEdit {
                    range: Range {
                        start: Position {
                            line: 0,
                            character: 0
                        },
                        end: Position {
                            line: 0,
                            character: 0
                        },
                    },
                    new_text: "// Formatted by mock\n".to_string(),
                }])
            }
            "workspace/symbol" => {
                #[allow(deprecated)]
                let syms = vec![SymbolInformation {
                    name: "mock_symbol".to_string(),
                    kind: SymbolKind::FUNCTION,
                    tags: None,
                    deprecated: None,
                    location: Location {
                        uri: Uri::from_str("file:///src/lib.rs").unwrap(),
                        range: Range {
                            start: Position {
                                line: 1,
                                character: 0,
                            },
                            end: Position {
                                line: 1,
                                character: 10,
                            },
                        },
                    },
                    container_name: None,
                }];
                json!(syms)
            }
            "shutdown" => json!(null),
            _ => {
                return Response {
                    jsonrpc: "2.0".to_string(),
                    id: Some(req.id.clone()),
                    result: None,
                    error: Some(crate::jsonrpc::ResponseError {
                        code: -32601,
                        message: format!("Method {} not found", req.method),
                        data: None,
                    }),
                };
            }
        };

        Response {
            jsonrpc: "2.0".to_string(),
            id: Some(req.id.clone()),
            result: Some(result),
            error: None,
        }
    }
}
