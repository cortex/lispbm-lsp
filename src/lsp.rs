use std::collections::HashMap;
use std::path;

use tokio::sync::Mutex;
use tower_lsp_server::jsonrpc::Result;
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
    pub parser: Mutex<tree_sitter::Parser>,
    pub state: Mutex<state::State>,
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
                let mut state = self.state.lock().await;
                for folder in folders {
                    let path: path::PathBuf = folder.uri.to_file_path().unwrap().into();
                    state.root = path;
                    self.client
                        .log_message(
                            MessageType::INFO,
                            format!("Workspace folder: {}", folder.uri.path()),
                        )
                        .await;
                }
            })
            .unwrap()
            .await;
        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::FULL,
                )),
                ..Default::default()
            },
            ..Default::default()
        })
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let path: path::PathBuf = params.text_document.uri.to_file_path().unwrap().into();
        if let Some(p) = self.check_entry_file(&path).await {
            self.parse_entry_file(&p).await;
        };

        self.on_change(params.text_document.uri, params.text_document.text)
            .await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        // For simplicity, we assume TextDocumentSyncKind::FULL
        if let Some(event) = params.content_changes.first() {
            self.parse_definitions(&params.text_document.uri, &event.text)
                .await;

            self.on_change(params.text_document.uri, event.text.clone())
                .await;
        }
    }
    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }
}

impl Backend {
    async fn on_change(&self, uri: Uri, text: String) {
        let mut parser = self.parser.lock().await;
        let tree = parser.parse(&text, None).unwrap();

        let mut diagnostics = Vec::new();
        collect_syntax_errors(tree.root_node(), &mut diagnostics);

        self.client
            .publish_diagnostics(uri, diagnostics, None)
            .await;
    }

    async fn parse_entry_file(&self, path: &path::Path) {
        let state = self.state.lock().await;
        let mut parser = self.parser.lock().await;
        let entry = match state.entry_files.get(path) {
            Some(entry) => entry,
            None => {
                self.client
                    .log_message(
                        MessageType::ERROR,
                        format!("No entry file found for: {}", path.display()),
                    )
                    .await;
                return;
            }
        };
        if let Some(import_paths) = entry.get_all_imports(&mut parser).await {
            self.client
                .log_message(
                    MessageType::INFO,
                    format!(
                        "Parsed entry file: {} with imports: {:#?}",
                        path.display(),
                        import_paths,
                    ),
                )
                .await;
        } else {
            self.client
                .log_message(
                    MessageType::ERROR,
                    format!("Failed to parse entry file: {}", path.display()),
                )
                .await;
        }
    }

    async fn check_entry_file(&self, uri: &path::Path) -> Option<path::PathBuf> {
        let mut state = self.state.lock().await;
        if let Some(entry_path) = entry::EntryFile::find_closest_entry_file(uri, &state.root) {
            match entry::EntryFile::load_from_file(&entry_path) {
                Ok(entry) => {
                    self.client
                        .log_message(
                            MessageType::INFO,
                            format!("Loaded entry file: {}", entry_path.display()),
                        )
                        .await;
                    if state
                        .entry_files
                        .insert(entry_path.clone(), entry)
                        .is_none()
                    {
                        Some(entry_path)
                    } else {
                        None
                    }
                }
                Err(e) => {
                    self.client
                        .log_message(
                            MessageType::ERROR,
                            format!("Failed to load entry file: {}", e),
                        )
                        .await;
                    None
                }
            }
        } else {
            None
        }
    }

    async fn parse_definitions(&self, uri: &Uri, text: &str) {
        let mut state = self.state.lock().await;
        let mut parser = self.parser.lock().await;
        let path = &uri.to_file_path().unwrap();
        let entry_path =
            match entry::EntryFile::find_closest_entry_file(path.parent().unwrap(), &state.root) {
                Some(s) => s,
                None => {
                    self.client
                        .log_message(
                            MessageType::ERROR,
                            format!(
                                "No entry file found for: {}: {}",
                                uri.path(),
                                path.display()
                            ),
                        )
                        .await;
                    return;
                }
            };

        let tree = parser.parse(text, None).unwrap();

        let q = tree_sitter::Query::new(
            &tree_sitter_lispbm::LANGUAGE.into(),
            r#"
            (_
                (comment)* @doc_comment
                .
                (definition name: (symbol) @name )
                (comment)* @doc_comment)
            "#,
        )
        .unwrap();

        let mut cursor = QueryCursor::new();
        let root = tree.root_node();
        let mut defs = HashMap::<String, definitions::Definition>::new();
        cursor.matches(&q, root, text.as_bytes()).for_each(|m| {
            let (name, def) = definitions::Definition::from_def_match(
                m.captures,
                text.as_bytes(),
                path.to_path_buf(),
            )
            .unwrap();
            defs.insert(name, def);
        });

        self.client
            .log_message(
                MessageType::INFO,
                format!("Parsed definitions for: {}: {:#?}", uri.path(), defs),
            )
            .await;
        state.definitions.insert(entry_path, defs);
    }
}
