use std::{collections::HashMap, path, str, vec};

use derivative::Derivative;
use tokio::sync;
use tower_lsp_server::ls_types::{self, DiagnosticSeverity, lsif::Edge::Diagnostic};
use tracing::dispatcher;
use tree_sitter::{QueryCursor, StreamingIterator};

use crate::{
    definitions::{self, Definition},
    entry,
};

#[derive(Debug, Hash, Clone, PartialEq, Eq)]
pub struct EntryId(pub path::PathBuf);

impl From<path::PathBuf> for EntryId {
    fn from(value: path::PathBuf) -> Self {
        EntryId(value)
    }
}

impl AsRef<path::Path> for EntryId {
    fn as_ref(&self) -> &path::Path {
        &self.0
    }
}

impl From<String> for EntryId {
    fn from(value: String) -> Self {
        EntryId(path::PathBuf::from(value))
    }
}

#[derive(Debug)]
pub enum Request {
    SetRoot {
        path: path::PathBuf,
    },
    NewEntry {
        id: EntryId,
    },
    GetDefinition {
        file: path::PathBuf,
        line: u32,
        column: u32,
        response: sync::oneshot::Sender<Vec<ls_types::Location>>,
    },
    GetHover {
        file: path::PathBuf,
        line: u32,
        column: u32,
        response: sync::oneshot::Sender<Option<ls_types::Hover>>,
    },
    UpdateDefinitions {
        file: path::PathBuf,
    },
    GetDiagnostics {
        file: path::PathBuf,
        content: String,
        update: bool,
        response: sync::oneshot::Sender<Vec<ls_types::Diagnostic>>,
    },
}

#[derive(Debug)]
pub struct EntryData {
    pub file: entry::EntryFile,
    pub definitions: HashMap<String, Definition>,
}

#[derive(Debug)]
pub struct File {
    pub entry_files: Vec<EntryId>,
    pub tree: tree_sitter::Tree,
}

#[derive(Derivative)]
#[derivative(Debug)]
pub struct State {
    rx: sync::mpsc::Receiver<Request>,
    pub entry_files: HashMap<EntryId, EntryData>,
    pub files: HashMap<path::PathBuf, File>,
    #[derivative(Debug = "ignore")]
    pub parser: tree_sitter::Parser,
    pub root: path::PathBuf,
}

impl State {
    pub fn new(rx: sync::mpsc::Receiver<Request>) -> Self {
        let mut parser = tree_sitter::Parser::new();
        let language = tree_sitter_lispbm::LANGUAGE;
        parser
            .set_language(&language.into())
            .expect("Error loading lispBM grammar");

        State {
            rx,
            entry_files: HashMap::new(),
            parser,
            files: HashMap::new(),
            root: path::PathBuf::new(),
        }
    }

    async fn new_entry(&mut self, id: EntryId) {
        let entry_file = entry::EntryFile::load_from_file(&id.0).await.unwrap();
        let imports = entry_file.get_all_imports(&mut self.parser).await.unwrap();

        for import in imports {
            let content = tokio::fs::read_to_string(&import).await.unwrap();
            let tree = self.parser.parse(&content, None).unwrap();
            let defs =
                definitions::Definition::parse_definitions(&tree, &import, content.as_bytes())
                    .unwrap();

            self.files
                .entry(import)
                .and_modify(|f| f.entry_files.push(id.clone()))
                .or_insert(File {
                    entry_files: vec![id.clone()],
                    tree,
                });

            match self.entry_files.get_mut(&id) {
                Some(e) => e.definitions.extend(defs),
                None => {
                    self.entry_files.insert(
                        id.clone(),
                        EntryData {
                            file: entry_file.clone(),
                            definitions: defs,
                        },
                    );
                }
            }
        }
    }

    async fn get_diagnostics(
        &mut self,
        file: path::PathBuf,
        content: String,
    ) -> Vec<ls_types::Diagnostic> {
        let file = match self.files.get(&file) {
            Some(f) => f,
            None => return vec![],
        };

        let q = tree_sitter::Query::new(
            &tree_sitter_lispbm::LANGUAGE.into(),
            r#"
            [
              (ERROR) @err
              (MISSING) @mis
            ]
            "#,
        )
        .unwrap();

        let mut cursor = QueryCursor::new();
        let root = file.tree.root_node();
        let mut diagnostics = vec![];
        cursor.matches(&q, root, content.as_bytes()).for_each(|m| {
            for cap in m.captures {
                let node = cap.node;
                let start_position = node.start_position();
                let end_position = node.end_position();
                diagnostics.push(ls_types::Diagnostic {
                    range: ls_types::Range {
                        start: ls_types::Position {
                            line: start_position.row as u32,
                            character: start_position.column as u32,
                        },
                        end: ls_types::Position {
                            line: end_position.row as u32,
                            character: end_position.column as u32,
                        },
                    },
                    severity: Some(DiagnosticSeverity::ERROR),
                    message: format!(
                        "{} error: {}",
                        node.kind(),
                        cap.node.utf8_text(content.as_bytes()).unwrap_or("".into())
                    ),
                    ..Default::default()
                });
            }
        });

        diagnostics
    }

    async fn update_definitions(&mut self, file: path::PathBuf) {
        if let Some(f) = self.files.get(&file) {
            let content = tokio::fs::read_to_string(&file).await.unwrap();
            let defs =
                definitions::Definition::parse_definitions(&f.tree, &file, content.as_bytes())
                    .unwrap();

            for (name, def) in defs.into_iter() {
                for entry_id in &f.entry_files {
                    if let Some(entry_data) = self.entry_files.get_mut(entry_id) {
                        entry_data.definitions.insert(name.clone(), def.clone());
                    }
                }
            }
        }
    }

    async fn get_definition(
        &self,
        content: &[u8],
        line: u32,
        column: u32,
        file: &File,
    ) -> Vec<&Definition> {
        let node = file.tree.root_node().descendant_for_point_range(
            tree_sitter::Point {
                row: line as usize,
                column: column as usize,
            },
            tree_sitter::Point {
                row: line as usize,
                column: column as usize,
            },
        );

        let mut total_defs = vec![];

        if let Some(node) = node {
            let name = node.utf8_text(content);
            if let Ok(name) = name {
                for entry_id in &file.entry_files {
                    if let Some(entry_data) = self.entry_files.get(entry_id) {
                        if let Some(def) = entry_data.definitions.get(name) {
                            total_defs.push(def);
                        }
                    }
                }
            }
        }

        total_defs
    }

    fn update_tree(&mut self, path: &path::PathBuf, content: &str) {
        let tree = self.parser.parse(content, None).unwrap();
        if let Some(f) = self.files.get_mut(path) {
            f.tree = tree;
        }
    }

    pub async fn run(&mut self) {
        while let Some(request) = self.rx.recv().await {
            match request {
                Request::SetRoot { path } => {
                    self.root = path;
                }
                Request::NewEntry { id } => {
                    self.new_entry(id).await;
                }
                Request::GetDefinition {
                    file,
                    line,
                    column,
                    response,
                } => {
                    let content = tokio::fs::read_to_string(&file).await.unwrap();
                    let file = self.files.get(&file).unwrap();
                    let locations = self
                        .get_definition(content.as_bytes(), line, column, file)
                        .await
                        .iter()
                        .map(|d| ls_types::Location {
                            uri: ls_types::Uri::from_file_path(&d.file).unwrap(),
                            range: ls_types::Range {
                                start: ls_types::Position {
                                    line: d.line,
                                    character: d.column,
                                },
                                end: ls_types::Position {
                                    line: d.line,
                                    character: d.column + d.len,
                                },
                            },
                        })
                        .collect::<Vec<_>>();
                    let _ = response.send(locations);
                }
                Request::GetHover {
                    file,
                    line,
                    column,
                    response,
                } => {
                    let content = tokio::fs::read_to_string(&file).await.unwrap();
                    let file = self.files.get(&file).unwrap();
                    let defs = self
                        .get_definition(content.as_bytes(), line, column, file)
                        .await;
                    if defs.is_empty() {
                        let _ = response.send(None);
                        continue;
                    }
                    let len = defs.iter().map(|d| d.len).max().unwrap_or(0);
                    let hover_text = defs
                        .iter()
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
                        .join("\n");
                    let hover = ls_types::Hover {
                        contents: ls_types::HoverContents::Scalar(ls_types::MarkedString::String(
                            hover_text.clone(),
                        )),
                        range: Some(ls_types::Range {
                            start: ls_types::Position {
                                line,
                                character: column,
                            },
                            end: ls_types::Position {
                                line,
                                character: column + len,
                            },
                        }),
                    };
                    let _ = response.send(Some(hover));
                }
                Request::UpdateDefinitions { file } => {
                    self.update_definitions(file).await;
                }
                Request::GetDiagnostics {
                    file,
                    content,
                    update,
                    response,
                } => {
                    if update {
                        self.update_tree(&file, &content);
                    }
                    let diagnostics = self.get_diagnostics(file, content).await;
                    let _ = response.send(diagnostics);
                }
            }
        }
    }
}
