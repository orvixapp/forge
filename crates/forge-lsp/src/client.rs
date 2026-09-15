//! High-level asynchronous LSP client with request correlation, cancellation and notifications.

use crate::jsonrpc::{Id, Message, Notification, Request, Response, ResponseError};
use crate::transport::{TransportError, read_message, write_message};
use lsp_types::*;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncWrite, BufReader};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::task::JoinHandle;
use tracing::{debug, error, trace, warn};

#[derive(Debug, Error)]
pub enum LspError {
    #[error("Transport error: {0}")]
    Transport(#[from] TransportError),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("RPC error: {0}")]
    Rpc(#[from] ResponseError),
    #[error("Request timed out")]
    Timeout,
    #[error("Request cancelled")]
    Cancelled,
    #[error("Server connection closed")]
    Closed,
    #[error("Channel error: {0}")]
    Channel(String),
}

type PendingRequests = Arc<tokio::sync::Mutex<HashMap<Id, oneshot::Sender<Result<Value, ResponseError>>>>>;

#[derive(Clone)]
pub struct LspClient {
    next_id: Arc<AtomicU64>,
    outgoing_tx: mpsc::Sender<Message>,
    pending_requests: PendingRequests,
    notifications_tx: broadcast::Sender<Notification>,
    _reader_handle: Arc<JoinHandle<()>>,
    _writer_handle: Arc<JoinHandle<()>>,
}

impl LspClient {
    /// Connects to a language server over the given async reader and writer.
    pub fn new<R, W>(reader: R, mut writer: W) -> Self
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (outgoing_tx, mut outgoing_rx) = mpsc::channel::<Message>(128);
        let (notifications_tx, _) = broadcast::channel::<Notification>(256);
        let pending_requests: PendingRequests = Arc::new(tokio::sync::Mutex::new(HashMap::new()));

        let pending_for_reader = Arc::clone(&pending_requests);
        let notif_tx_for_reader = notifications_tx.clone();
        let outgoing_tx_for_reader = outgoing_tx.clone();

        // Background reader task
        let reader_handle = tokio::spawn(async move {
            let mut buf_reader = BufReader::new(reader);
            loop {
                match read_message(&mut buf_reader).await {
                    Ok(payload) => {
                        match Message::parse(&payload) {
                            Ok(Message::Response(res)) => {
                                if let Some(id) = res.id {
                                    let mut pending = pending_for_reader.lock().await;
                                    if let Some(sender) = pending.remove(&id) {
                                        let result = if let Some(err) = res.error {
                                            Err(err)
                                        } else {
                                            Ok(res.result.unwrap_or(Value::Null))
                                        };
                                        let _ = sender.send(result);
                                    } else {
                                        trace!("Received response for unknown/cancelled id: {id:?}");
                                    }
                                }
                            }
                            Ok(Message::Notification(notif)) => {
                                trace!("Received LSP notification: {}", notif.method);
                                let _ = notif_tx_for_reader.send(notif);
                            }
                            Ok(Message::Request(req)) => {
                                debug!("Received server-to-client request: {}", req.method);
                                // Handle known server requests with sensible defaults
                                let response = match req.method.as_str() {
                                    "workspace/configuration" => Response {
                                        jsonrpc: "2.0".to_string(),
                                        id: Some(req.id),
                                        result: Some(json!([])),
                                        error: None,
                                    },
                                    "client/registerCapability" | "client/unregisterCapability" => Response {
                                        jsonrpc: "2.0".to_string(),
                                        id: Some(req.id),
                                        result: Some(Value::Null),
                                        error: None,
                                    },
                                    "window/workDoneProgress/create" => Response {
                                        jsonrpc: "2.0".to_string(),
                                        id: Some(req.id),
                                        result: Some(Value::Null),
                                        error: None,
                                    },
                                    _ => Response {
                                        jsonrpc: "2.0".to_string(),
                                        id: Some(req.id),
                                        result: None,
                                        error: Some(ResponseError {
                                            code: -32601,
                                            message: format!("Method {} not supported by client", req.method),
                                            data: None,
                                        }),
                                    },
                                };
                                let _ = outgoing_tx_for_reader.send(Message::Response(response)).await;
                            }
                            Err(err) => {
                                warn!("Failed to parse incoming LSP message: {err}");
                            }
                        }
                    }
                    Err(TransportError::Closed) => {
                        debug!("LSP transport connection closed");
                        break;
                    }
                    Err(err) => {
                        error!("LSP transport read error: {err}");
                        break;
                    }
                }
            }

            // Connection died: cancel all pending requests
            let mut pending = pending_for_reader.lock().await;
            for (_, tx) in pending.drain() {
                let _ = tx.send(Err(ResponseError {
                    code: -32099,
                    message: "LSP connection closed".to_string(),
                    data: None,
                }));
            }
        });

        // Background writer task
        let writer_handle = tokio::spawn(async move {
            while let Some(msg) = outgoing_rx.recv().await {
                match msg.to_bytes() {
                    Ok(bytes) => {
                        if let Err(err) = write_message(&mut writer, &bytes).await {
                            error!("Failed to write LSP message: {err}");
                            break;
                        }
                    }
                    Err(err) => {
                        error!("Failed to serialize LSP message: {err}");
                    }
                }
            }
        });

        Self {
            next_id: Arc::new(AtomicU64::new(1)),
            outgoing_tx,
            pending_requests,
            notifications_tx,
            _reader_handle: Arc::new(reader_handle),
            _writer_handle: Arc::new(writer_handle),
        }
    }

    /// Subscribes to server notifications.
    #[must_use]
    pub fn subscribe_notifications(&self) -> broadcast::Receiver<Notification> {
        self.notifications_tx.subscribe()
    }

    /// Sends a raw JSON-RPC request with default timeout (15s).
    pub async fn send_request_raw(
        &self,
        method: &str,
        params: Option<Value>,
    ) -> Result<Value, LspError> {
        self.send_request_timeout(method, params, Duration::from_secs(15)).await
    }

    /// Sends a raw JSON-RPC request with a custom timeout.
    pub async fn send_request_timeout(
        &self,
        method: &str,
        params: Option<Value>,
        timeout: Duration,
    ) -> Result<Value, LspError> {
        let id = Id::Number(self.next_id.fetch_add(1, Ordering::Relaxed) as i64);
        let (tx, rx) = oneshot::channel();

        {
            let mut pending = self.pending_requests.lock().await;
            pending.insert(id.clone(), tx);
        }

        let request = Message::Request(Request {
            jsonrpc: "2.0".to_string(),
            id: id.clone(),
            method: method.to_string(),
            params,
        });

        if let Err(err) = self.outgoing_tx.send(request).await {
            let mut pending = self.pending_requests.lock().await;
            pending.remove(&id);
            return Err(LspError::Channel(err.to_string()));
        }

        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(Ok(val))) => Ok(val),
            Ok(Ok(Err(err))) => Err(LspError::Rpc(err)),
            Ok(Err(_)) => Err(LspError::Closed),
            Err(_) => {
                // Timeout: remove from pending and send $/cancelRequest
                {
                    let mut pending = self.pending_requests.lock().await;
                    pending.remove(&id);
                }
                let _ = self.cancel_request(id).await;
                Err(LspError::Timeout)
            }
        }
    }

    /// Cancels a pending request via `$/cancelRequest`.
    pub async fn cancel_request(&self, id: Id) -> Result<(), LspError> {
        let cancel_params = CancelParams {
            id: match id {
                Id::Number(n) => NumberOrString::Number(n as i32),
                Id::String(s) => NumberOrString::String(s),
            },
        };
        self.send_notification("$/cancelRequest", Some(serde_json::to_value(cancel_params)?))
            .await
    }

    /// Sends a typed request and deserializes the typed response.
    pub async fn send_request<P: Serialize, R: DeserializeOwned>(
        &self,
        method: &str,
        params: P,
    ) -> Result<R, LspError> {
        let params_val = serde_json::to_value(params)?;
        let result_val = self.send_request_raw(method, Some(params_val)).await?;
        Ok(serde_json::from_value(result_val)?)
    }

    /// Sends a notification to the language server.
    pub async fn send_notification(
        &self,
        method: &str,
        params: Option<Value>,
    ) -> Result<(), LspError> {
        let notif = Message::Notification(Notification {
            jsonrpc: "2.0".to_string(),
            method: method.to_string(),
            params,
        });
        self.outgoing_tx
            .send(notif)
            .await
            .map_err(|err| LspError::Channel(err.to_string()))
    }

    /// Sends a typed notification to the language server.
    pub async fn send_typed_notification<P: Serialize>(
        &self,
        method: &str,
        params: P,
    ) -> Result<(), LspError> {
        let val = serde_json::to_value(params)?;
        self.send_notification(method, Some(val)).await
    }

    // --- Standard LSP lifecycle methods ---

    /// Performs the LSP `initialize` handshake.
    pub async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult, LspError> {
        self.send_request("initialize", params).await
    }

    /// Sends the `initialized` notification.
    pub async fn initialized(&self) -> Result<(), LspError> {
        self.send_typed_notification("initialized", InitializedParams {}).await
    }

    /// Performs the LSP `shutdown` request.
    pub async fn shutdown(&self) -> Result<(), LspError> {
        let _: Value = self.send_request("shutdown", ()).await?;
        Ok(())
    }

    /// Sends the LSP `exit` notification.
    pub async fn exit(&self) -> Result<(), LspError> {
        self.send_notification("exit", None).await
    }

    // --- Standard LSP text document synchronization ---

    pub async fn did_open(&self, params: DidOpenTextDocumentParams) -> Result<(), LspError> {
        self.send_typed_notification("textDocument/didOpen", params).await
    }

    pub async fn did_change(&self, params: DidChangeTextDocumentParams) -> Result<(), LspError> {
        self.send_typed_notification("textDocument/didChange", params).await
    }

    pub async fn did_save(&self, params: DidSaveTextDocumentParams) -> Result<(), LspError> {
        self.send_typed_notification("textDocument/didSave", params).await
    }

    pub async fn did_close(&self, params: DidCloseTextDocumentParams) -> Result<(), LspError> {
        self.send_typed_notification("textDocument/didClose", params).await
    }

    // --- Language features ---

    pub async fn completion(
        &self,
        params: CompletionParams,
    ) -> Result<Option<CompletionResponse>, LspError> {
        self.send_request("textDocument/completion", params).await
    }

    pub async fn resolve_completion_item(
        &self,
        item: CompletionItem,
    ) -> Result<CompletionItem, LspError> {
        self.send_request("completionItem/resolve", item).await
    }

    pub async fn hover(&self, params: HoverParams) -> Result<Option<Hover>, LspError> {
        self.send_request("textDocument/hover", params).await
    }

    pub async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>, LspError> {
        self.send_request("textDocument/definition", params).await
    }

    pub async fn references(
        &self,
        params: ReferenceParams,
    ) -> Result<Option<Vec<Location>>, LspError> {
        self.send_request("textDocument/references", params).await
    }

    pub async fn formatting(
        &self,
        params: DocumentFormattingParams,
    ) -> Result<Option<Vec<TextEdit>>, LspError> {
        self.send_request("textDocument/formatting", params).await
    }

    pub async fn range_formatting(
        &self,
        params: DocumentRangeFormattingParams,
    ) -> Result<Option<Vec<TextEdit>>, LspError> {
        self.send_request("textDocument/rangeFormatting", params).await
    }

    pub async fn code_action(
        &self,
        params: CodeActionParams,
    ) -> Result<Option<CodeActionResponse>, LspError> {
        self.send_request("textDocument/codeAction", params).await
    }

    pub async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> Result<Option<DocumentSymbolResponse>, LspError> {
        self.send_request("textDocument/documentSymbol", params).await
    }

    pub async fn workspace_symbols(
        &self,
        query: &str,
    ) -> Result<Option<Vec<SymbolInformation>>, LspError> {
        #[allow(deprecated)]
        let params = WorkspaceSymbolParams {
            query: query.to_string(),
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };
        self.send_request("workspace/symbol", params).await
    }
}
