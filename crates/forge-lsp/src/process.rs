//! Process supervision, LSP lifecycle and crash recovery with automatic re-didOpen.

use crate::capabilities::client_capabilities;
use crate::client::{LspClient, LspError};
use crate::diagnostics::DiagnosticStore;
use crate::registry::ServerConfig;
use crate::sync::{DocumentTracker, TrackedDocument, path_to_uri};
use lsp_types::*;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tracing::{debug, info, trace, warn};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerStatus {
    Unstarted,
    Starting,
    Running,
    Crashed,
    Stopping,
    Stopped,
}

pub struct ServerInstance {
    pub config: ServerConfig,
    pub root_path: PathBuf,
    pub root_uri: Uri,
    client: Option<LspClient>,
    child: Option<Child>,
    server_capabilities: Option<ServerCapabilities>,
    tracker: Arc<Mutex<DocumentTracker>>,
    diagnostics: DiagnosticStore,
    status: ServerStatus,
    last_activity: Instant,
    restart_count: u32,
}

impl ServerInstance {
    pub fn new(
        config: ServerConfig,
        root_path: PathBuf,
        diagnostics: DiagnosticStore,
    ) -> Result<Self, LspError> {
        let root_uri = path_to_uri(&root_path).ok_or_else(|| {
            LspError::Channel(format!("Failed to convert path to URI: {root_path:?}"))
        })?;

        Ok(Self {
            config,
            root_path,
            root_uri,
            client: None,
            child: None,
            server_capabilities: None,
            tracker: Arc::new(Mutex::new(DocumentTracker::new())),
            diagnostics,
            status: ServerStatus::Unstarted,
            last_activity: Instant::now(),
            restart_count: 0,
        })
    }

    #[must_use]
    pub fn status(&self) -> ServerStatus {
        self.status
    }

    #[must_use]
    pub fn client(&self) -> Option<&LspClient> {
        self.client.as_ref()
    }

    #[must_use]
    pub fn tracker(&self) -> Arc<Mutex<DocumentTracker>> {
        Arc::clone(&self.tracker)
    }

    #[must_use]
    pub fn server_capabilities(&self) -> Option<&ServerCapabilities> {
        self.server_capabilities.as_ref()
    }

    pub fn record_activity(&mut self) {
        self.last_activity = Instant::now();
    }

    #[must_use]
    pub fn is_idle(&self, idle_timeout: Duration) -> bool {
        self.status == ServerStatus::Running && self.last_activity.elapsed() >= idle_timeout
    }

    /// Spawns and initializes the server, performing handshake and re-syncing any open files.
    pub async fn start(&mut self) -> Result<(), LspError> {
        self.status = ServerStatus::Starting;
        info!(
            "Starting LSP server '{}' at {:?}",
            self.config.name, self.root_path
        );

        let mut cmd = Command::new(&self.config.command);
        cmd.args(&self.config.args)
            .envs(&self.config.env)
            .current_dir(&self.root_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = cmd.spawn().map_err(|e| {
            self.status = ServerStatus::Crashed;
            LspError::Transport(crate::transport::TransportError::Io(e))
        })?;

        let stdin = child.stdin.take().expect("Failed to open child stdin");
        let stdout = child.stdout.take().expect("Failed to open child stdout");
        let stderr = child.stderr.take().expect("Failed to open child stderr");

        let server_name = self.config.name.clone();
        // Background task to log stderr lines
        tokio::spawn(async move {
            use tokio::io::{AsyncBufReadExt, BufReader};
            let mut reader = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                debug!("[LSP stderr: {server_name}] {line}");
            }
        });

        let client = LspClient::new(stdout, stdin);

        // Subscribe to diagnostics notifications
        let mut notif_rx = client.subscribe_notifications();
        let diag_store = self.diagnostics.clone();
        let server_name_diag = self.config.name.clone();
        tokio::spawn(async move {
            while let Ok(notif) = notif_rx.recv().await {
                if notif.method == "textDocument/publishDiagnostics" {
                    if let Some(params_val) = notif.params {
                        match serde_json::from_value::<PublishDiagnosticsParams>(params_val) {
                            Ok(params) => {
                                trace!(
                                    "[{server_name_diag}] publishDiagnostics for {}: {} items",
                                    params.uri.as_str(),
                                    params.diagnostics.len()
                                );
                                diag_store.update(params.uri, params.version, params.diagnostics);
                            }
                            Err(e) => {
                                warn!("[{server_name_diag}] Malformed publishDiagnostics params: {e}");
                            }
                        }
                    }
                }
            }
        });

        // Initialize handshake
        #[allow(deprecated)]
        let init_params = InitializeParams {
            process_id: Some(std::process::id()),
            root_path: Some(self.root_path.to_string_lossy().into_owned()),
            root_uri: Some(self.root_uri.clone()),
            initialization_options: self.config.initialization_options.clone(),
            capabilities: client_capabilities(),
            trace: None,
            workspace_folders: Some(vec![WorkspaceFolder {
                uri: self.root_uri.clone(),
                name: self
                    .root_path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned(),
            }]),
            client_info: Some(ClientInfo {
                name: "Forge".to_string(),
                version: Some("0.0.1".to_string()),
            }),
            locale: None,
            work_done_progress_params: WorkDoneProgressParams::default(),
        };

        let init_result = match client.initialize(init_params).await {
            Ok(res) => res,
            Err(e) => {
                let _ = child.kill().await;
                self.status = ServerStatus::Crashed;
                return Err(e);
            }
        };

        client.initialized().await?;
        self.server_capabilities = Some(init_result.capabilities);

        // Re-didOpen any documents previously opened (crash recovery / restart)
        let docs: Vec<TrackedDocument> = {
            let tracker = self.tracker.lock().await;
            tracker.all_documents()
        };

        for doc in docs {
            let did_open_params = DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: doc.uri,
                    language_id: doc.language_id,
                    version: doc.version,
                    text: doc.text,
                },
            };
            if let Err(err) = client.did_open(did_open_params).await {
                warn!("Failed to re-sync document after restart: {err}");
            }
        }

        self.client = Some(client);
        self.child = Some(child);
        self.status = ServerStatus::Running;
        self.last_activity = Instant::now();
        info!("LSP server '{}' is running and ready", self.config.name);
        Ok(())
    }

    /// Stops the server gracefully with `shutdown` + `exit`, killing if unresponsive.
    pub async fn stop(&mut self) -> Result<(), LspError> {
        if self.status != ServerStatus::Running && self.status != ServerStatus::Starting {
            return Ok(());
        }

        self.status = ServerStatus::Stopping;
        info!("Stopping LSP server '{}'", self.config.name);

        if let Some(client) = self.client.take() {
            // Attempt clean shutdown with timeout
            let shutdown_res = tokio::time::timeout(Duration::from_secs(2), client.shutdown()).await;
            if shutdown_res.is_ok() {
                let _ = client.exit().await;
            }
        }

        if let Some(mut child) = self.child.take() {
            let wait_res = tokio::time::timeout(Duration::from_secs(1), child.wait()).await;
            if wait_res.is_err() {
                let _ = child.kill().await;
            }
        }

        self.status = ServerStatus::Stopped;
        Ok(())
    }

    /// Checks if the process crashed, and restarts with backoff if needed.
    pub async fn check_and_recover(&mut self) -> Result<bool, LspError> {
        let is_dead = if let Some(ref mut child) = self.child {
            match child.try_wait() {
                Ok(Some(status)) => {
                    warn!("LSP server '{}' exited with {status}", self.config.name);
                    true
                }
                Ok(None) => false,
                Err(err) => {
                    warn!("Error polling LSP child process: {err}");
                    true
                }
            }
        } else {
            self.status == ServerStatus::Crashed
        };

        if is_dead {
            self.status = ServerStatus::Crashed;
            self.child = None;
            self.client = None;

            // Exponential backoff: 200ms, 400ms, 800ms, ... capped at 5s
            let backoff_ms = (200u64 * (1 << self.restart_count.min(5))).min(5000);
            info!(
                "Recovering LSP server '{}' in {backoff_ms}ms (restart #{})",
                self.config.name, self.restart_count + 1
            );
            tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
            self.restart_count += 1;
            self.start().await?;
            return Ok(true);
        }

        Ok(false)
    }
}
