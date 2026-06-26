use std::{
    collections::{HashMap, HashSet},
    path, str, vec,
};

use derivative::Derivative;
use tokio::sync;
use tower_lsp_server::ls_types::{self, DiagnosticSeverity};
use tracing::{error, info, warn};
use tree_sitter::{QueryCursor, StreamingIterator};

use crate::{
    builtin,
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
    pub tree: Option<tree_sitter::Tree>,
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

        info!("[State] Initialized with lispBM grammar");

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
                error!("[Entry] Error loading {:?}: {}", id.0, e);
                return;
            }
        };
        let mut imports = entry_file.get_all_imports(&mut self.parser).await.unwrap();
        imports.push(entry_file.entry_point.clone());
        let ext_imports = match entry_file.get_ext_imports() {
            Ok(i) => i,
            Err(e) => {
                error!(
                    "[Entry] Error getting external imports for {:?}: {}",
                    id.0, e
                );
                vec![]
            }
        };
        imports.extend(ext_imports);
        imports.push(id.0.clone());
        info!("[Entry] New entry: {:?}, imports: {:?}", id, imports);

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
        let root = file.tree.as_ref().unwrap().root_node();
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
            let ext = file
                .as_ref()
                .extension()
                .and_then(|s| s.to_str())
                .unwrap_or("");
            let mut defs = match ext {
                "lisp" | "lsp" => definitions::Definition::parse_definitions(
                    f.tree.as_ref().unwrap(),
                    file.as_ref(),
                    content.as_bytes(),
                )
                .unwrap(),
                "toml" => {
                    let filename = file.as_ref().file_name().and_then(|s| s.to_str());
                    match filename {
                        Some("entry.toml") => {
                            let entry = match entry::EntryFile::load_from_file(file.as_ref()).await
                            {
                                Ok(s) => s,
                                Err(e) => {
                                    error!("Error loading entry {:?}: {}", file.0, e);
                                    return;
                                }
                            };

                            let ext_imports = entry.get_ext_inline_definitions();

                            info!(
                                "[Definition] Updated entry file {:?} with {} definitions",
                                file,
                                ext_imports.len()
                            );

                            self.entry_files.insert(EntryId(file.0.clone()), entry);

                            ext_imports
                        }
                        _ => {
                            let def_file: entry::DefinitionFile = match toml::from_str(&content) {
                                Ok(d) => d,
                                Err(e) => {
                                    error!(
                                        "Failed to parse definition collection file {:?}: {}",
                                        file, e
                                    );
                                    return;
                                }
                            };
                            def_file.get_definitions(file.as_ref())
                        }
                    }
                }
                _ => {
                    warn!("Unsupported file extension {:?} for file {:?}", ext, file);
                    return;
                }
            };

            info!(
                "[Definition] Cleaning old definitions, {} definitions removed, file: {:?}",
                defs.len(),
                &file,
            );
            for symbol in defs.keys() {
                if let Some(file_defs) = self.symbol_index.get_mut(symbol) {
                    file_defs.remove(&file);
                }
            }

            info!(
                "[Definition] Updated definitions, {} definitions added, file: {:?}",
                defs.len(),
                &file,
            );
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
        let node = node_at(file.tree.as_ref(), line, column);

        let node_text = node.as_ref().map(|n| n.utf8_text(file.content.as_bytes()));
        info!(
            "[Definition] Getting {:?} at {}:{}",
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
            info!("[Tree] Updating file {:?}", path);
            f.tree = Some(tree);
        }
    }

    pub async fn run(&mut self) {
        while let Some(request) = self.rx.recv().await {
            match request {
                Request::SetRoot { path } => {
                    self.root = path;
                    info!("[Root] Set path {:?}", self.root);
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
                            warn!("[Definition] {e}");
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
                    info!("[Definition] Updating file {:?}", &file);
                    self.update_definitions(file.into(), content).await;
                }
                Request::GetDiagnostics {
                    file,
                    content,
                    update,
                    response,
                } => {
                    if self.root.as_path() == path::Path::new("") {
                        warn!(
                            "[Diagnostics] Root path not set, skipping diagnostics for {:?}",
                            &file
                        );
                        let _ = response.send(vec![]);
                        continue;
                    }
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
        info!("[Hover] Request for {:?} at {}:{}", file, line, column);
        let file = self.files.get(&file).ok_or("File not found")?;
        let defs = self.get_definition(line, column, file).await;
        if defs.is_empty() {
            info!("[Hover] No definitions found at {}:{}", line, column);
            return Ok(None);
        }
        let node_under_pos = match node_at(file.tree.as_ref(), line, column) {
            Some(s) => s,
            None => {
                info!("[Hover] No node found at {}:{}", line, column);
                return Ok(None);
            }
        };
        let line = node_under_pos.start_position().row as u32;
        let column = node_under_pos.start_position().column as u32;
        let len = node_under_pos.end_position().column as u32 - column;
        let hover_text = defs
            .iter()
            .map(|def| {
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

                match def
                    .comment
                    .as_ref()
                    .map(|comment| format!("{}\n\n__{}__", comment, filename))
                {
                    Some(s) => s,
                    None => format!("\n\n__{}__", filename),
                }
            })
            .collect::<Vec<_>>()
            .join("\n\n---\n\n");
        info!("[Hover] at: {}:{} text: {:?}", line, column, hover_text);
        let hover = ls_types::Hover {
            contents: ls_types::HoverContents::Markup(ls_types::MarkupContent {
                kind: ls_types::MarkupKind::Markdown,
                value: hover_text,
            }),
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
            "[Definition] Found {:?} line {}, column {}",
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
            let ext = file
                .as_ref()
                .extension()
                .and_then(|s| s.to_str())
                .unwrap_or("");
            let content = match ext {
                "lisp" | "lsp" => match tokio::fs::read_to_string(file).await {
                    Ok(c) => c,
                    Err(e) => {
                        error!("Failed to read file {:?}: {}", file, e);
                        continue;
                    }
                },
                "toml" => {
                    let filename = file.as_ref().file_name().and_then(|s| s.to_str());
                    match filename {
                        Some("entry.toml") => match tokio::fs::read_to_string(file).await {
                            Ok(c) => c,
                            Err(e) => {
                                error!("Failed to read entry file {:?}: {}", file, e);
                                continue;
                            }
                        },
                        Some(s) if s.ends_with(".builtin.ext.toml") => {
                            match builtin::Builtin::from_filename(s) {
                                Some(builtin) => {
                                    info!("[Indexing] Loading builtin definition file {:?}", file);
                                    builtin.get_ref().to_string()
                                }
                                None => {
                                    warn!(
                                        "[Indexing] Unsupported builtin definition file {:?}",
                                        file
                                    );
                                    continue;
                                }
                            }
                        }
                        _ => match tokio::fs::read_to_string(file).await {
                            Ok(c) => {
                                info!("[Indexing] Loading definition collection file {:?}", file);
                                c
                            }
                            Err(e) => {
                                error!(
                                    "Failed to read definition collection file {:?}: {}",
                                    file, e
                                );
                                continue;
                            }
                        },
                    }
                }
                _ => {
                    warn!(
                        "[Indexing] Unsupported file extension {:?} for file {:?}",
                        ext, file
                    );
                    continue;
                }
            };

            let ext = file
                .as_ref()
                .extension()
                .and_then(|s| s.to_str())
                .unwrap_or("");
            info!("[Indexing] file: {:?} type: {}", &file, &ext);
            let defs = match ext {
                "lisp" | "lsp" => {
                    let tree = self.parser.parse(&content, None).unwrap();
                    let defs = definitions::Definition::parse_definitions(
                        &tree,
                        &file.0,
                        content.as_bytes(),
                    )
                    .unwrap();
                    info!(
                        "[Indexing] Indexed file {:?} for entry {:?} with {} definitions",
                        &file,
                        id.0.file_name().unwrap().display(),
                        defs.len()
                    );

                    self.files
                        .entry(file.clone())
                        .and_modify(|f| f.definitions = defs.clone())
                        .or_insert(File {
                            content,
                            tree: Some(tree),
                            definitions: defs.clone(),
                        });

                    defs
                }
                "toml" => {
                    let filename = file.as_ref().file_name().and_then(|s| s.to_str());
                    match filename {
                        Some("entry.toml") => {
                            let entry = match entry::EntryFile::load_from_file(file.as_ref()).await
                            {
                                Ok(s) => s,
                                Err(e) => {
                                    error!("Error loading entry {:?}: {}", id.0, e);
                                    continue;
                                }
                            };
                            let ext_imports = entry.get_ext_inline_definitions();
                            info!(
                                "[Indexing] Indexed entry file {:?} with {} definitions",
                                &file,
                                ext_imports.len()
                            );
                            self.files
                                .entry(file.clone())
                                .and_modify(|f| f.definitions = ext_imports.clone())
                                .or_insert(File {
                                    content,
                                    tree: None,
                                    definitions: ext_imports.clone(),
                                });
                            self.entry_files.insert(id.clone(), entry);

                            ext_imports
                        }
                        _ => {
                            let def_file: entry::DefinitionFile = match toml::from_str(&content) {
                                Ok(d) => d,
                                Err(e) => {
                                    error!(
                                        "Failed to parse definition collection file {:?}: {}",
                                        file, e
                                    );
                                    continue;
                                }
                            };
                            let defs = def_file.get_definitions(file.as_ref());

                            info!(
                                "[Indexing] Indexed definition collection file {:?} with {} definitions",
                                &file,
                                defs.len()
                            );

                            self.files
                                .entry(file.clone())
                                .and_modify(|f| f.definitions = defs.clone())
                                .or_insert(File {
                                    content,
                                    tree: None,
                                    definitions: defs.clone(),
                                });

                            defs
                        }
                    }
                }
                _ => {
                    warn!("Unsupported file extension {:?} for file {:?}", ext, file);
                    return;
                }
            };

            for (name, def) in defs {
                self.symbol_index
                    .entry(name)
                    .or_default()
                    .insert(file.clone(), def);
            }
        }
    }
}

fn node_at(
    tree: Option<&tree_sitter::Tree>,
    line: u32,
    column: u32,
) -> Option<tree_sitter::Node<'_>> {
    if let Some(tree) = tree {
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
    } else {
        None
    }
}
