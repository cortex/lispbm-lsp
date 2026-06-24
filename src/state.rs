use std::{collections::HashMap, path, str, vec};

use derivative::Derivative;
use tokio::sync;
use tower_lsp_server::ls_types::{self, DiagnosticSeverity};
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
pub struct EntryData {
    pub file: entry::EntryFile,
    pub definitions: HashMap<String, Vec<Definition>>,
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
        let entry_file = match entry::EntryFile::load_from_file(&id.0).await {
            Ok(s) => s,
            Err(e) => {
                error!("Error loading entry {:?}: {}", id.0, e);
                return;
            }
        };
        let imports = entry_file.get_all_imports(&mut self.parser).await.unwrap();
        info!("New entry: {:?}, imports: {:?}", id, imports);

        self.import_file(&entry_file, entry_file.entry_point.clone(), &id)
            .await;

        for import in imports {
            self.import_file(&entry_file, import, &id).await;
        }

        let mut ext_defs = entry_file.get_all_ext_definitions().await;
        match self.entry_files.get_mut(&id) {
            Some(e) => e.definitions.iter_mut().for_each(|(name, def)| {
                if let Some(new_def) = ext_defs.remove(name) {
                    info!("Added ext defs: {:?}", &new_def,);
                    def.extend(new_def);
                }
            }),
            None => {
                info!("Added new ext defs: {:?}", &ext_defs,);
                self.entry_files.insert(
                    id.clone(),
                    EntryData {
                        file: entry_file.clone(),
                        definitions: ext_defs,
                    },
                );
            }
        }
        info!(
            "Added ext definitions for entry {:?} with {} definitions",
            id,
            self.entry_files.get(&id).unwrap().definitions.len()
        );
    }

    async fn import_file(
        &mut self,
        entry_file: &entry::EntryFile,
        import: path::PathBuf,
        id: &EntryId,
    ) {
        let content = tokio::fs::read_to_string(&import).await.unwrap();
        let tree = self.parser.parse(&content, None).unwrap();
        let mut defs =
            definitions::Definition::parse_definitions(&tree, &import, content.as_bytes()).unwrap();
        info!(
            "Adding definitions for {} in entry {:?} with {} definitions",
            import.file_name().unwrap().display(),
            id.0.file_name().unwrap().display(),
            defs.len()
        );

        self.files
            .entry(import)
            .and_modify(|f| f.entry_files.push(id.clone()))
            .or_insert(File {
                entry_files: vec![id.clone()],
                tree,
            });

        match self.entry_files.get_mut(id) {
            Some(e) => e.definitions.iter_mut().for_each(|(name, def)| {
                if let Some(new_def) = defs.remove(name) {
                    def.extend(new_def);
                }
            }),
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

    async fn update_definitions(&mut self, file: path::PathBuf, content: &[u8]) {
        if let Some(f) = self.files.get(&file) {
            let mut defs =
                definitions::Definition::parse_definitions(&f.tree, &file, content).unwrap();

            for entry_id in &f.entry_files {
                if let Some(entry_data) = self.entry_files.get_mut(entry_id) {
                    for (name, def) in entry_data.definitions.iter_mut() {
                        def.retain(|d| !d.source.is_file(&file));
                        if let Some(new_def) = defs.remove(name) {
                            def.extend(new_def);
                        }
                    }

                    // Add new definitions that are present in the file but not in the entry file
                    for (name, new_def) in defs.iter() {
                        entry_data
                            .definitions
                            .entry(name.clone())
                            .or_insert_with(Vec::new)
                            .extend(new_def.clone());
                    }

                    info!(
                        "Updated definitions for entry {:?} with total {} definitions",
                        entry_id,
                        entry_data.definitions.len()
                    )
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

        let node_text = node.as_ref().map(|n| n.utf8_text(content));
        info!(
            "Getting definition for node {:?} at line {}, column {}",
            node_text, line, column
        );

        let mut total_defs = vec![];

        if let Some(node) = node {
            let name = node.utf8_text(content);
            if let Ok(name) = name {
                for entry_id in &file.entry_files {
                    if let Some(entry_data) = self.entry_files.get(entry_id)
                        && let Some(def) = entry_data.definitions.get(name)
                    {
                        total_defs.extend(def);
                    }
                }
            }
        }

        total_defs
    }

    fn update_tree(&mut self, path: &path::PathBuf, content: &str) {
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
                    let locations = match self.handle_definition(file, line, column).await {
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
                    let hover = match self.handle_hover(file, line, column).await {
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
                    self.update_definitions(file, content.as_bytes()).await;
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

    async fn handle_hover(
        &mut self,
        file: path::PathBuf,
        line: u32,
        column: u32,
    ) -> Result<Option<ls_types::Hover>, String> {
        info!(
            "Handling hover request for file {:?} at line {}, column {}",
            file, line, column
        );
        let content = tokio::fs::read_to_string(&file)
            .await
            .map_err(|e| e.to_string())?;
        let file = self.files.get(&file).ok_or("File not found")?;
        let defs = self
            .get_definition(content.as_bytes(), line, column, file)
            .await;
        if defs.is_empty() {
            info!("No definitions found for hover at {}:{}", line, column);
            return Ok(None);
        }
        let node_under_pos = node_at(&file.tree, line, column);
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
                        let entry_file_path = file
                            .entry_files
                            .first()
                            .map(|id| &id.0)
                            .unwrap_or(&self.root);
                        pathdiff::diff_paths(path, entry_file_path)
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
        file: path::PathBuf,
        line: u32,
        column: u32,
    ) -> Result<Vec<ls_types::Location>, String> {
        let content = tokio::fs::read_to_string(&file)
            .await
            .map_err(|e| e.to_string())?;
        let file = self.files.get(&file).ok_or("File not found in state")?;
        let locations = self
            .get_definition(content.as_bytes(), line, column, file)
            .await;

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
}

fn node_at(tree: &tree_sitter::Tree, line: u32, column: u32) -> tree_sitter::Node<'_> {
    tree.root_node()
        .descendant_for_point_range(
            tree_sitter::Point {
                row: line as usize,
                column: column as usize,
            },
            tree_sitter::Point {
                row: line as usize,
                column: column as usize,
            },
        )
        .unwrap()
}
