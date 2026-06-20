use std::collections::{HashMap, hash_map};
use std::os::linux::raw::stat;
use std::path;

use tokio::sync::{self, Mutex};
use tower_lsp_server::jsonrpc::{self, Result};
use tower_lsp_server::ls_types::*;
use tower_lsp_server::{Client, LanguageServer};
use tree_sitter::{Node, QueryCursor, StreamingIterator, StreamingIteratorMut};

use crate::{definitions, entry, state};

pub fn collect_syntax_errors(node: Node, diagnostics: &mut Vec<Diagnostic>) {
    if node.is_error() || node.is_missing() {
        let range = Range {
            start: Position::new(
                node.start_position().row as u32,
                node.start_position().column as u32,
            ),
            end: Position::new(
                node.end_position().row as u32,
                node.end_position().column as u32,
            ),
        };

        diagnostics.push(Diagnostic {
            range,
            severity: Some(DiagnosticSeverity::ERROR),
            message: format!("Syntax error: unexpected {}", node.kind()),
            ..Default::default()
        });
    }

    // Recursively check children
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_syntax_errors(child, diagnostics);
    }
}

pub struct Backend {
    pub client: Client,
    pub state: sync::mpsc::Sender<state::Request>,
}

impl LanguageServer for Backend {
    async fn initialize(&self, p: InitializeParams) -> Result<InitializeResult> {
        self.client
            .log_message(
                MessageType::INFO,
                "LispBM LSP Server initialized".to_string(),
            )
            .await;
        p.workspace_folders
            .as_ref()
            .map(async |folders| {
                for folder in folders {
                    let path: path::PathBuf = folder.uri.to_file_path().unwrap().into();
                    self.client
                        .log_message(
                            MessageType::INFO,
                            format!("Workspace folder: {}", folder.uri.path()),
                        )
                        .await;

                    self.state
                        .send(state::Request::SetRoot { path: path.clone() })
                        .await
                        .unwrap();

                    let globmatch =
                        glob::glob(path.join("**/entry.toml").to_str().unwrap()).unwrap();

                    for entry in globmatch {
                        match entry {
                            Ok(entry_path) => {
                                let req = state::Request::NewEntry {
                                    id: entry_path.into(),
                                };
                                self.state.send(req).await.unwrap();
                            }
                            Err(e) => {
                                self.client
                                    .log_message(
                                        MessageType::ERROR,
                                        format!("Error finding entry files: {}", e),
                                    )
                                    .await;
                            }
                        }
                    }
                }
            })
            .unwrap()
            .await;

        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::FULL,
                )),
                definition_provider: Some(OneOf::Left(true)),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                ..Default::default()
            },
            ..Default::default()
        })
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let path: path::PathBuf = params.text_document.uri.to_file_path().unwrap().into();
        let content = params.text_document.text.clone();

        let (tx, rx) = tokio::sync::oneshot::channel();
        self.state
            .send(state::Request::GetDiagnostics {
                file: path,
                content,
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
                self.client
                    .log_message(
                        MessageType::ERROR,
                        format!("Error occurred while fetching diagnostics: {}", e),
                    )
                    .await;
            }
        }
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        self.client
            .log_message(
                MessageType::INFO,
                format!(
                    "Received goto type definition request for: {} at line {}, column {}",
                    params
                        .text_document_position_params
                        .text_document
                        .uri
                        .path(),
                    params.text_document_position_params.position.line,
                    params.text_document_position_params.position.character
                ),
            )
            .await;
        let path = params
            .text_document_position_params
            .text_document
            .uri
            .to_file_path()
            .unwrap();

        let line = params.text_document_position_params.position.line;
        let column = params.text_document_position_params.position.character;

        let (tx, rx) = tokio::sync::oneshot::channel();
        self.state
            .send(state::Request::GetDefinition {
                file: path.into(),
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
        let path = params
            .text_document_position_params
            .text_document
            .uri
            .path();

        let line = params.text_document_position_params.position.line;
        let column = params.text_document_position_params.position.character;

        let (tx, rx) = tokio::sync::oneshot::channel();
        self.state
            .send(state::Request::GetHover {
                file: path.as_str().into(),
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
        let path: path::PathBuf = params.text_document.uri.to_file_path().unwrap().into();

        self.state
            .send(state::Request::UpdateDefinitions { file: path })
            .await
            .unwrap();
    }

    async fn did_change(&self, mut params: DidChangeTextDocumentParams) {
        // For simplicity, we assume TextDocumentSyncKind::FULL
        if let Some(event) = params.content_changes.pop() {
            let path: path::PathBuf = params.text_document.uri.to_file_path().unwrap().into();

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
                    self.client
                        .log_message(
                            MessageType::ERROR,
                            format!("Error occurred while fetching diagnostics: {}", e),
                        )
                        .await;
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
        let (tx, rx) = sync::mpsc::channel::<state::Request>(16);

        let mut state = state::State::new(rx);
        tokio::spawn(async move {
            state.run().await;
        });

        Self { client, state: tx }
    }
}
