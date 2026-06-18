use std::collections::{HashMap, hash_map};
use std::os::linux::raw::stat;
use std::path;

use tokio::sync::Mutex;
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
                    self.client
                        .log_message(
                            MessageType::INFO,
                            format!("Workspace folder: {}", folder.uri.path()),
                        )
                        .await;

                    let globmatch =
                        glob::glob(path.join("**/entry.toml").to_str().unwrap()).unwrap();

                    let mut parser = &mut self.parser.lock().await;

                    for entry in globmatch {
                        match entry {
                            Ok(entry_path) => {
                                if let Ok(entry_file) =
                                    entry::EntryFile::load_from_file(&entry_path)
                                {
                                    self.client
                                        .log_message(
                                            MessageType::INFO,
                                            format!("Loaded entry file: {}", entry_path.display()),
                                        )
                                        .await;
                                    Self::parse_entry_file(
                                        &mut state,
                                        &mut parser,
                                        &self.client,
                                        &entry_file,
                                        &entry_path,
                                    )
                                    .await;
                                    state
                                        .entry_files
                                        .insert(state::EntryId(entry_path.clone()), entry_file);
                                } else {
                                    self.client
                                        .log_message(
                                            MessageType::ERROR,
                                            format!(
                                                "Failed to load entry file: {}",
                                                entry_path.display()
                                            ),
                                        )
                                        .await;
                                }
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

                    state.root = path;
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
        let state = self.state.lock().await;
        if !state.file_to_entry.contains_key(&path) {
            // if file is not part of any entry file, re-index workspace, could be a new file created in the workspace
            self.client
                .log_message(
                    MessageType::INFO,
                    format!(
                        "TODO: Opened file {} is not part of any entry file, re-indexing workspace",
                        path.display()
                    ),
                )
                .await;
        }

        self.on_change(params.text_document.uri, params.text_document.text)
            .await;
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
        let state = self.state.lock().await;
        let mut parser = self.parser.lock().await;
        let path = params
            .text_document_position_params
            .text_document
            .uri
            .to_file_path()
            .ok_or(jsonrpc::Error::new(jsonrpc::ErrorCode::ParseError))?;
        let entry_ids = state
            .file_to_entry
            .get(&path.to_path_buf())
            .ok_or(jsonrpc::Error::new(jsonrpc::ErrorCode::ParseError))?;

        let mut total_defs = vec![];

        for entry_id in entry_ids {
            self.client
                .log_message(
                    MessageType::INFO,
                    format!(
                        "File {} is part of entry file {}",
                        path.display(),
                        entry_id.0.display()
                    ),
                )
                .await;
            let defs = state
                .definitions
                .get(entry_id)
                .ok_or(jsonrpc::Error::new(jsonrpc::ErrorCode::ParseError))?;
            let content = std::fs::read_to_string(&path)
                .ok()
                .ok_or(jsonrpc::Error::new(jsonrpc::ErrorCode::ParseError))?;
            let tree = parser
                .parse(content.as_bytes(), None)
                .ok_or(jsonrpc::Error::new(jsonrpc::ErrorCode::ParseError))?;
            let pos = params.text_document_position_params.position;

            let point = tree_sitter::Point {
                row: pos.line as usize,
                column: pos.character as usize,
            };

            let node = tree.root_node().descendant_for_point_range(point, point);
            self.client
                .log_message(
                    MessageType::INFO,
                    format!(
                        "Found node at position: kind='{}', is_error={}, is_missing={}",
                        node.as_ref().map(|n| n.kind()).unwrap_or("<none>"),
                        node.as_ref().map(|n| n.is_error()).unwrap_or(false),
                        node.as_ref().map(|n| n.is_missing()).unwrap_or(false),
                    ),
                )
                .await;
            let node = node.ok_or(jsonrpc::Error::new(jsonrpc::ErrorCode::ParseError))?;
            let symbol = node.utf8_text(content.as_bytes());
            self.client
                .log_message(
                    MessageType::INFO,
                    format!(
                        "Node at position: kind='{}', text='{}'",
                        node.kind(),
                        symbol.as_ref().unwrap_or(&"<invalid utf8>"),
                    ),
                )
                .await;
            let symbol = symbol
                .ok()
                .ok_or(jsonrpc::Error::new(jsonrpc::ErrorCode::ParseError))?;
            self.client
                .log_message(
                    MessageType::INFO,
                    format!(
                        "Looking for definition of symbol '{}' in entry file: {}",
                        symbol,
                        entry_id.0.display()
                    ),
                )
                .await;

            let def = defs.get(symbol);
            self.client
                .log_message(
                    MessageType::INFO,
                    format!(
                        "Definition lookup for symbol '{}': {:?} defs: {:#?}",
                        symbol, def, defs,
                    ),
                )
                .await;

            let def = def.ok_or(jsonrpc::Error::new(jsonrpc::ErrorCode::ServerError(1)))?;

            let loc = Location {
                uri: Uri::from_file_path(&def.file).unwrap(),
                range: Range {
                    start: Position {
                        line: def.line,
                        character: def.column,
                    },
                    end: Position {
                        line: def.line,
                        character: def.column + symbol.len() as u32,
                    },
                },
            };

            self.client
                .log_message(
                    MessageType::INFO,
                    format!(
                        "Found definition for symbol '{}': {} at line {}, column {}",
                        symbol,
                        def.file.display(),
                        def.line,
                        def.column
                    ),
                )
                .await;
            total_defs.push(loc);
        }

        Ok(Some(request::GotoTypeDefinitionResponse::Array(total_defs)))
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let path = params
            .text_document_position_params
            .text_document
            .uri
            .path()
            .to_string();
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        let state = self.state.lock().await;
        let mut parser = self.parser.lock().await;
        let tree = parser.parse(text.as_bytes(), None).unwrap();
        let pos = params.text_document_position_params.position;
        let node = tree
            .root_node()
            .descendant_for_point_range(
                tree_sitter::Point {
                    row: pos.line as usize,
                    column: pos.character as usize,
                },
                tree_sitter::Point {
                    row: pos.line as usize,
                    column: pos.character as usize,
                },
            )
            .ok_or(jsonrpc::Error::new(jsonrpc::ErrorCode::ParseError))?;

        let start = node.start_position();
        let end = node.end_position();
        let symbol = node.utf8_text(text.as_bytes()).unwrap_or("<invalid utf8>");

        let defs = state
            .file_to_entry
            .get(&path::PathBuf::from(path))
            .map(|entry_ids| {
                entry_ids
                    .iter()
                    .filter_map(|entry_id| {
                        state
                            .definitions
                            .get(entry_id)
                            .and_then(|defs| defs.get(symbol))
                    })
                    .collect::<Vec<&definitions::Definition>>()
            })
            .map(|defs| {
                defs.iter()
                    .filter_map(|def| {
                        def.comment.as_ref().map(|comment| {
                            format!(
                                "{}\n\n__{}__",
                                comment,
                                def.file.file_name().unwrap().display(),
                            )
                        })
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .map(|d| Hover {
                contents: HoverContents::Scalar(MarkedString::String(d)),
                range: Some(Range {
                    start: Position {
                        line: start.row as u32,
                        character: start.column as u32,
                    },
                    end: Position {
                        line: end.row as u32,
                        character: end.column as u32,
                    },
                }),
            });

        self.client
            .log_message(
                MessageType::INFO,
                format!(
                    "Received hover request for: {} at line {}, column {}",
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
        Ok(defs)
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let mut state = self.state.lock().await;
        let mut parser = self.parser.lock().await;
        // For simplicity, we assume TextDocumentSyncKind::FULL
        if let Some(event) = params.content_changes.first() {
            let path: path::PathBuf = params.text_document.uri.to_file_path().unwrap().into();
            let entry_ids = state.file_to_entry.get(&path).unwrap_or(&vec![]).clone();
            for entry_id in entry_ids {
                self.client
                    .log_message(
                        MessageType::INFO,
                        format!(
                            "File {} is part of entry file {}, re-parsing",
                            path.display(),
                            entry_id.0.display()
                        ),
                    )
                    .await;
                let defs = Self::parse_definitions(
                    &mut parser,
                    &self.client,
                    &params.text_document.uri,
                    &event.text,
                )
                .await;
                match state.definitions.get_mut(&entry_id) {
                    Some(h) => {
                        h.extend(defs);
                    }
                    None => {
                        state.definitions.insert(entry_id.clone(), defs);
                    }
                };
            }

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

    async fn parse_entry_file(
        state: &mut state::State,
        parser: &mut tree_sitter::Parser,
        client: &Client,
        entry_file: &entry::EntryFile,
        entry_path: &path::Path,
    ) {
        if let Some(import_paths) = entry_file.get_all_imports(parser).await {
            client
                .log_message(
                    MessageType::INFO,
                    format!(
                        "Found {} imports in entry file {}: {:#?}",
                        import_paths.len(),
                        entry_path.display(),
                        import_paths,
                    ),
                )
                .await;
            for import_path in import_paths {
                client
                    .log_message(
                        MessageType::INFO,
                        format!("Parsing imported file: {}", import_path.display()),
                    )
                    .await;
                if let Ok(content) = std::fs::read_to_string(&import_path) {
                    let defs = Self::parse_definitions(
                        parser,
                        client,
                        &Uri::from_file_path(&import_path).unwrap(),
                        &content,
                    )
                    .await;
                    let entry_id = state::EntryId(entry_path.to_path_buf());
                    match state.definitions.get_mut(&entry_id) {
                        Some(h) => {
                            h.extend(defs);
                        }
                        None => {
                            state.definitions.insert(entry_id.clone(), defs);
                        }
                    };
                    state
                        .file_to_entry
                        .entry(import_path.clone())
                        .or_default()
                        .push(entry_id);
                } else {
                    client
                        .log_message(
                            MessageType::ERROR,
                            format!("Failed to read imported file: {}", import_path.display()),
                        )
                        .await;
                }
            }
        } else {
            client
                .log_message(
                    MessageType::ERROR,
                    format!("Failed to parse entry file: {}", entry_path.display()),
                )
                .await;
        }
    }

    async fn parse_definitions(
        parser: &mut tree_sitter::Parser,
        client: &Client,
        uri: &Uri,
        text: &str,
    ) -> HashMap<String, definitions::Definition> {
        let path = &uri.to_file_path().unwrap();

        let tree = parser.parse(text, None).unwrap();

        let q = tree_sitter::Query::new(
            &tree_sitter_lispbm::LANGUAGE.into(),
            r#"
            (_
                (comment)* @doc_comment
                .
                [
                  (definition name: (symbol) @name ) @node
                  (function_definition name: (symbol) @name ) @node
                ]
                .
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

        client
            .log_message(
                MessageType::INFO,
                format!("Parsed definitions for: {}: {:#?}", uri.path(), defs),
            )
            .await;

        defs
    }
}
