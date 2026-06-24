use std::{
    collections::{HashMap, HashSet},
    path, str, vec,
};

use derivative::Derivative;
use tokio::sync;
use tower_lsp_server::ls_types::{self, DiagnosticSeverity, MonikerKind::Import};
use tracing::{error, info, warn};
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

#[derive(Debug, Hash, Clone, PartialEq, Eq)]
pub struct FileId(pub path::PathBuf);

impl From<path::PathBuf> for FileId {
    fn from(value: path::PathBuf) -> Self {
        FileId(value)
    }
}

impl AsRef<path::Path> for FileId {
    fn as_ref(&self) -> &path::Path {
        &self.0
    }
}

impl From<String> for FileId {
    fn from(value: String) -> Self {
        FileId(path::PathBuf::from(value))
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
        content: String,
    },
    GetDiagnostics {
        file: path::PathBuf,
        content: String,
        update: bool,
        response: sync::oneshot::Sender<Vec<ls_types::Diagnostic>>,
    },
}

#[derive(Debug)]
pub struct File {
    pub content: String,
    pub tree: tree_sitter::Tree,
    pub definitions: HashMap<String, Vec<Definition>>,
}

#[derive(Derivative)]
#[derivative(Debug)]
pub struct State {
    rx: sync::mpsc::Receiver<Request>,
    pub entry_files: HashMap<EntryId, entry::EntryFile>,
    pub entry_to_files: HashMap<EntryId, HashSet<FileId>>,
    pub symbol_index: HashMap<String, HashMap<FileId, Vec<Definition>>>,
    pub files: HashMap<FileId, File>,
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
            entry_to_files: HashMap::new(),
            symbol_index: HashMap::new(),
            parser,
            files: HashMap::new(),
            root: path::PathBuf::new(),
        }
    }

    async fn new_entry(&mut self, id: EntryId) {
        let entry_file = match entry::EntryFile::load_from_file(&id.0).await {
            Ok(s) => s,
            Err(e) => {
                error!("Error loading entry {:?}: {}", id.0, e);
                return;
            }
        };
        let mut imports = entry_file.get_all_imports(&mut self.parser).await.unwrap();
        imports.push(entry_file.entry_point.clone());
        info!("New entry: {:?}, imports: {:?}", id, imports);

        self.import_files(imports, &id);

        self.index_files(&id).await;
    }

    async fn get_diagnostics(
        &mut self,
        file: FileId,
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
                        "{}: {}",
                        node.kind(),
                        cap.node.utf8_text(content.as_bytes()).unwrap_or("")
                    ),
                    ..Default::default()
                });
            }
        });

        diagnostics
    }

    async fn update_definitions(&mut self, file: FileId, content: String) {
        if let Some(f) = self.files.get_mut(&file) {
            let mut defs = definitions::Definition::parse_definitions(
                &f.tree,
                file.as_ref(),
                content.as_bytes(),
            )
            .unwrap();

            info!(
                "Cleaning up old definitions for file {:?}, removing {} definitions",
                &file,
                defs.len()
            );
            for symbol in defs.keys() {
                if let Some(file_defs) = self.symbol_index.get_mut(symbol) {
                    file_defs.remove(&file);
                }
            }

            for (name, def) in defs.drain() {
                self.symbol_index
                    .entry(name)
                    .or_default()
                    .insert(file.clone(), def);
            }

            f.definitions = defs;
            f.content = content;
        }
    }

    async fn get_definition(&self, line: u32, column: u32, file: &File) -> Vec<&Definition> {
        let node = node_at(&file.tree, line, column);

        let node_text = node.as_ref().map(|n| n.utf8_text(file.content.as_bytes()));
        info!(
            "Getting definition for node {:?} at line {}, column {}",
            node_text, line, column
        );

        let mut total_defs = vec![];

        if let Some(node) = node
            && let Ok(symbol) = node.utf8_text(file.content.as_bytes())
            && let Some(defs) = self.symbol_index.get(symbol)
        {
            for defs in defs.values() {
                total_defs.extend(defs.iter());
            }
        }

        total_defs
    }

    fn update_tree(&mut self, path: &FileId, content: &str) {
        let tree = self.parser.parse(content, None).unwrap();
        if let Some(f) = self.files.get_mut(path) {
            info!("Updating tree for file {:?}", path);
            f.tree = tree;
        }
    }

    pub async fn run(&mut self) {
        while let Some(request) = self.rx.recv().await {
            match request {
                Request::SetRoot { path } => {
                    self.root = path;
                    info!("Set root path to {:?}", self.root);
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
                    let locations = match self.handle_definition(file.into(), line, column).await {
                        Ok(l) => l,
                        Err(e) => {
                            warn!("{e}");
                            let _ = response.send(vec![]);
                            continue;
                        }
                    };
                    let _ = response.send(locations);
                }
                Request::GetHover {
                    file,
                    line,
                    column,
                    response,
                } => {
                    let hover = match self.handle_hover(file.into(), line, column).await {
                        Ok(h) => h,
                        Err(e) => {
                            warn!("{e}");
                            let _ = response.send(None);
                            continue;
                        }
                    };
                    let _ = response.send(hover);
                }
                Request::UpdateDefinitions { file, content } => {
                    info!("Updating definitions for file {:?}", &file);
                    self.update_definitions(file.into(), content).await;
                }
                Request::GetDiagnostics {
                    file,
                    content,
                    update,
                    response,
                } => {
                    let file_id = FileId::from(file);
                    if update {
                        self.update_tree(&file_id, &content);
                    }
                    let diagnostics = self.get_diagnostics(file_id, content).await;
                    let _ = response.send(diagnostics);
                }
            }
        }
    }

    async fn handle_hover(
        &mut self,
        file: FileId,
        line: u32,
        column: u32,
    ) -> Result<Option<ls_types::Hover>, String> {
        info!(
            "Handling hover request for file {:?} at line {}, column {}",
            file, line, column
        );
        let file = self.files.get(&file).ok_or("File not found")?;
        let defs = self.get_definition(line, column, file).await;
        if defs.is_empty() {
            info!("No definitions found for hover at {}:{}", line, column);
            return Ok(None);
        }
        let node_under_pos = match node_at(&file.tree, line, column) {
            Some(s) => s,
            None => {
                info!("No node found at {}:{}", line, column);
                return Ok(None);
            }
        };
        let line = node_under_pos.start_position().row as u32;
        let column = node_under_pos.start_position().column as u32;
        let len = node_under_pos.end_position().column as u32 - column;
        let hover_text = defs
            .iter()
            .filter_map(|def| {
                let filename = match &def.source {
                    definitions::SourceInfo::Source { file, .. } => file
                        .file_name()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_else(|| file.display().to_string()),
                    definitions::SourceInfo::Builtin { name } => name.to_string(),
                    definitions::SourceInfo::Collection { path } => {
                        pathdiff::diff_paths(path, &self.root)
                            .unwrap_or(path.clone())
                            .display()
                            .to_string()
                    }
                };

                def.comment
                    .as_ref()
                    .map(|comment| format!("{}\n\n__{}__", comment, filename))
            })
            .collect::<Vec<_>>()
            .join("\n\n---\n\n");
        info!("Hover text for {}:{} is: {:?}", line, column, hover_text);
        let hover = ls_types::Hover {
            contents: ls_types::HoverContents::Scalar(ls_types::MarkedString::String(hover_text)),
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
        Ok(Some(hover))
    }

    async fn handle_definition(
        &mut self,
        file: FileId,
        line: u32,
        column: u32,
    ) -> Result<Vec<ls_types::Location>, String> {
        let file = self.files.get(&file).ok_or("File not found in state")?;
        let locations = self.get_definition(line, column, file).await;

        info!(
            "Found {:?} definitions at line {}, column {}",
            locations, line, column
        );

        let locations = locations
            .iter()
            .filter_map(|d| match &d.source {
                definitions::SourceInfo::Source {
                    file,
                    line,
                    column,
                    len,
                } => Some(ls_types::Location {
                    uri: ls_types::Uri::from_file_path(file).unwrap(),
                    range: ls_types::Range {
                        start: ls_types::Position {
                            line: *line,
                            character: *column,
                        },
                        end: ls_types::Position {
                            line: *line,
                            character: column + len,
                        },
                    },
                }),
                _ => None,
            })
            .collect::<Vec<_>>();

        Ok(locations)
    }

    fn import_files(&mut self, imports: Vec<path::PathBuf>, id: &EntryId) {
        for import in imports {
            self.entry_to_files
                .entry(id.clone())
                .or_default()
                .insert(import.into());
        }
    }

    async fn index_files(&mut self, id: &EntryId) {
        for file in self
            .entry_to_files
            .get(id)
            .unwrap_or(&HashSet::new())
            .iter()
        {
            let content = match tokio::fs::read_to_string(file).await {
                Ok(c) => c,
                Err(e) => {
                    error!("Failed to read file {:?}: {}", file, e);
                    continue;
                }
            };
            let tree = self.parser.parse(&content, None).unwrap();
            let defs =
                definitions::Definition::parse_definitions(&tree, &file.0, content.as_bytes())
                    .unwrap();
            info!(
                "Indexed file {:?} for entry {:?} with {} definitions",
                &file,
                id.0.file_name().unwrap().display(),
                defs.len()
            );

            self.files
                .entry(file.clone())
                .and_modify(|f| f.definitions = defs.clone())
                .or_insert(File {
                    content,
                    tree,
                    definitions: defs.clone(),
                });

            for (name, def) in defs {
                self.symbol_index
                    .entry(name)
                    .or_default()
                    .insert(file.clone(), def);
            }
        }
    }
}

fn node_at(tree: &tree_sitter::Tree, line: u32, column: u32) -> Option<tree_sitter::Node<'_>> {
    tree.root_node().descendant_for_point_range(
        tree_sitter::Point {
            row: line as usize,
            column: column as usize,
        },
        tree_sitter::Point {
            row: line as usize,
            column: column as usize,
        },
    )
}
