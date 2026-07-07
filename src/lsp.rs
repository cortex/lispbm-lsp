use std::path;

use tokio::sync;
use tower_lsp_server::jsonrpc::{self, Result};
use tower_lsp_server::ls_types::*;
use tower_lsp_server::{Client, LanguageServer};
use tracing::info;

use crate::state;

pub struct Backend {
    pub client: Client,
    pub state: sync::mpsc::Sender<state::Request>,
}

impl LanguageServer for Backend {
    async fn initialize(&self, p: InitializeParams) -> Result<InitializeResult> {
        let folders = p.workspace_folders.unwrap_or_default();
        for folder in folders {
            let Some(path): Option<path::PathBuf> = folder.uri.to_file_path().map(|p| p.into())
            else {
                tracing::error!("Failed to convert URI to file path: {}", folder.uri.path());
                continue;
            };
            info!("Workspace folder: {}", folder.uri.path());

            self.state
                .send(state::Request::SetRoot { path: path.clone() })
                .await
                .unwrap();

            let Ok(globmatch) = glob::glob(path.join("**/entry.toml").to_str().unwrap()) else {
                tracing::error!(
                    "Failed to create glob pattern for entry files in: {}",
                    path.display()
                );
                continue;
            };

            for entry in globmatch {
                match entry {
                    Ok(entry_path) => {
                        let req = state::Request::NewEntry {
                            id: entry_path.into(),
                        };
                        self.state.send(req).await.unwrap();
                    }
                    Err(e) => {
                        tracing::error!("Error finding entry files: {}", e);
                        continue;
                    }
                }
            }
        }

        info!("LispBM LSP Server capabilities sent");

        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Options(
                    TextDocumentSyncOptions {
                        open_close: Some(true),
                        change: Some(TextDocumentSyncKind::FULL),
                        will_save: Some(false),
                        will_save_wait_until: Some(false),
                        save: Some(TextDocumentSyncSaveOptions::SaveOptions(SaveOptions {
                            include_text: Some(true),
                        })),
                    },
                )),
                definition_provider: Some(OneOf::Left(true)),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                ..Default::default()
            },
            ..Default::default()
        })
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let Some(path): Option<path::PathBuf> =
            params.text_document.uri.to_file_path().map(|p| p.into())
        else {
            tracing::error!(
                "Failed to convert URI to file path: {}",
                params.text_document.uri.path()
            );
            return;
        };

        if path.is_file() && path.exists() {
            info!("File closed: {}", path.display());
        } else {
            self.state
                .send(state::Request::RemoveDefinitions { file: path })
                .await
                .unwrap();
        }
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let Some(path): Option<path::PathBuf> =
            params.text_document.uri.to_file_path().map(|p| p.into())
        else {
            tracing::error!(
                "Failed to convert URI to file path: {}",
                params.text_document.uri.path()
            );
            return;
        };
        let content = params.text_document.text;

        let (tx, rx) = tokio::sync::oneshot::channel();
        self.state
            .send(state::Request::GetDiagnostics {
                file: path.clone(),
                content: content.clone(),
                update: true,
                response: tx,
            })
            .await
            .unwrap();

        match rx.await {
            Ok(diagnostics) => {
                self.client
                    .publish_diagnostics(params.text_document.uri, diagnostics, None)
                    .await;
            }
            Err(e) => {
                tracing::error!("Error occurred while fetching diagnostics: {}", e);
            }
        }

        self.state
            .send(state::Request::UpdateDefinitions {
                file: path,
                content,
            })
            .await
            .unwrap();
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        info!(
            "Received goto type definition request for: {} at line {}, column {}",
            params
                .text_document_position_params
                .text_document
                .uri
                .path(),
            params.text_document_position_params.position.line,
            params.text_document_position_params.position.character
        );
        let path: path::PathBuf = params
            .text_document_position_params
            .text_document
            .uri
            .to_file_path()
            .map(|p| p.into())
            .ok_or(jsonrpc::Error::invalid_request())
            .inspect_err(|_| {
                tracing::error!(
                    "File not found for goto definition: {}",
                    params
                        .text_document_position_params
                        .text_document
                        .uri
                        .path()
                )
            })?;

        let line = params.text_document_position_params.position.line;
        let column = params.text_document_position_params.position.character;

        let (tx, rx) = tokio::sync::oneshot::channel();
        self.state
            .send(state::Request::GetDefinition {
                file: path,
                line,
                column,
                response: tx,
            })
            .await
            .unwrap();

        let total_defs = rx.await.unwrap_or_default();

        Ok(Some(request::GotoTypeDefinitionResponse::Array(total_defs)))
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let path: path::PathBuf = params
            .text_document_position_params
            .text_document
            .uri
            .to_file_path()
            .map(|p| p.into())
            .ok_or(jsonrpc::Error::invalid_request())
            .inspect_err(|_| {
                tracing::error!(
                    "File not found for hover: {}",
                    params
                        .text_document_position_params
                        .text_document
                        .uri
                        .path()
                )
            })?;

        let line = params.text_document_position_params.position.line;
        let column = params.text_document_position_params.position.character;

        self.client
            .log_message(
                MessageType::INFO,
                format!(
                    "Received hover request for: {} at line {}, column {}",
                    path.display(),
                    line,
                    column
                ),
            )
            .await;

        let (tx, rx) = tokio::sync::oneshot::channel();
        self.state
            .send(state::Request::GetHover {
                file: path,
                line,
                column,
                response: tx,
            })
            .await
            .unwrap();

        let res = rx.await.unwrap();

        Ok(res)
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        info!("File saved");
        let Some(path): Option<path::PathBuf> =
            params.text_document.uri.to_file_path().map(|f| f.into())
        else {
            tracing::error!(
                "File not found for save: {}",
                params.text_document.uri.path()
            );
            return;
        };
        let content = params.text.unwrap_or_default();

        self.state
            .send(state::Request::UpdateDefinitions {
                file: path,
                content,
            })
            .await
            .unwrap();
    }

    async fn did_change(&self, mut params: DidChangeTextDocumentParams) {
        // For simplicity, we assume TextDocumentSyncKind::FULL
        if let Some(event) = params.content_changes.pop() {
            let Some(path): Option<path::PathBuf> =
                params.text_document.uri.to_file_path().map(|f| f.into())
            else {
                tracing::error!("File not found: {}", params.text_document.uri.path());
                return;
            };

            let (tx, rx) = tokio::sync::oneshot::channel();
            self.state
                .send(state::Request::GetDiagnostics {
                    file: path,
                    content: event.text,
                    update: true,
                    response: tx,
                })
                .await
                .unwrap();

            match rx.await {
                Ok(diagnostics) => {
                    self.client
                        .publish_diagnostics(params.text_document.uri, diagnostics, None)
                        .await;
                }
                Err(e) => {
                    tracing::error!("Error occurred while fetching diagnostics: {}", e);
                }
            }
        }
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }
}

impl Backend {
    pub fn new(client: Client) -> Self {
        let (tx, rx) = sync::mpsc::channel::<state::Request>(32);

        let mut state = state::State::new(rx);
        tokio::spawn(async move {
            state.run().await;
        });

        Self { client, state: tx }
    }
}
