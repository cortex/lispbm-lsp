use std::path;

use tokio::sync::{mpsc, oneshot};
use tower_lsp_server::jsonrpc::{self, Result};
use tower_lsp_server::ls_types::*;
use tower_lsp_server::{Client, LanguageServer};
use tracing::{error, info};

use crate::file::FileId;
use crate::state::{Request, State};

pub struct Backend {
    client: Client,
    state: mpsc::Sender<Request>,
}

/// Handlers may run concurrently, so requests must be sent to the state before awaiting anything
/// else in order to be processed in the order they were received.
impl LanguageServer for Backend {
    async fn initialize(&self, p: InitializeParams) -> Result<InitializeResult> {
        for folder in p.workspace_folders.unwrap_or_default() {
            if let Some(root) = file_path(&folder.uri) {
                self.send(Request::OpenWorkspace { root }).await;
            }
        }

        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Options(
                    TextDocumentSyncOptions {
                        open_close: Some(true),
                        change: Some(TextDocumentSyncKind::FULL),
                        will_save: Some(false),
                        will_save_wait_until: Some(false),
                        save: Some(TextDocumentSyncSaveOptions::Supported(true)),
                    },
                )),
                definition_provider: Some(OneOf::Left(true)),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                ..Default::default()
            },
            ..Default::default()
        })
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let document = params.text_document;
        self.update_document(document.uri, document.version, document.text)
            .await;
    }

    async fn did_change(&self, mut params: DidChangeTextDocumentParams) {
        let document = params.text_document;
        // The last change is the whole document, since we only support TextDocumentSyncKind::FULL
        if let Some(change) = params.content_changes.pop() {
            self.update_document(document.uri, document.version, change.text)
                .await;
        }
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        if let Some(file) = file_id(&params.text_document.uri) {
            self.send(Request::DocumentSaved { file }).await;
        }
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        if let Some(file) = file_id(&params.text_document.uri) {
            self.send(Request::DocumentClosed { file }).await;
        }
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let (file, position) = file_position(params.text_document_position_params)?;
        info!("[Definition] Request for {:?} at {:?}", file, position);
        let locations = self
            .ask(|response| Request::GetDefinition {
                file,
                position,
                response,
            })
            .await;
        Ok(locations.map(GotoDefinitionResponse::Array))
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let (file, position) = file_position(params.text_document_position_params)?;
        info!("[Hover] Request for {:?} at {:?}", file, position);
        let hover = self
            .ask(|response| Request::GetHover {
                file,
                position,
                response,
            })
            .await;
        Ok(hover.flatten())
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }
}

impl Backend {
    pub fn new(client: Client) -> Self {
        let (tx, rx) = mpsc::channel(32);
        tokio::spawn(State::new().run(rx));
        Self { client, state: tx }
    }

    async fn send(&self, request: Request) {
        if self.state.send(request).await.is_err() {
            error!("[Server] State is no longer running");
        }
    }

    /// Sends a request and waits for the response. `None` means the state is no longer running.
    async fn ask<T>(&self, request: impl FnOnce(oneshot::Sender<T>) -> Request) -> Option<T> {
        let (tx, rx) = oneshot::channel();
        self.send(request(tx)).await;
        rx.await.ok()
    }

    async fn update_document(&self, uri: Uri, version: i32, text: String) {
        let Some(file) = file_id(&uri) else {
            return;
        };
        let diagnostics = self
            .ask(|response| Request::DocumentUpdated {
                file,
                text,
                response,
            })
            .await;
        if let Some(diagnostics) = diagnostics {
            self.client
                .publish_diagnostics(uri, diagnostics, Some(version))
                .await;
        }
    }
}

fn file_path(uri: &Uri) -> Option<path::PathBuf> {
    let path = uri.to_file_path().map(|path| path.into_owned());
    if path.is_none() {
        error!("Failed to convert URI to file path: {}", uri.path());
    }
    path
}

fn file_id(uri: &Uri) -> Option<FileId> {
    file_path(uri).map(|path| FileId::from_path(&path))
}

fn file_position(params: TextDocumentPositionParams) -> Result<(FileId, Position)> {
    let file = file_id(&params.text_document.uri).ok_or_else(jsonrpc::Error::invalid_request)?;
    Ok((file, params.position))
}
