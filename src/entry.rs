use std::{collections::HashMap, path};

use serde::{Deserialize, Serialize};
use tracing::{error, info};
use tree_sitter::{QueryCursor, StreamingIterator};

use crate::{
    builtin,
    definitions::{self, SourceInfo},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntryFile {
    pub entry_point: path::PathBuf,
    pub extension: Vec<Extension>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Extension {
    #[serde(rename = "path")]
    Collection(path::PathBuf),
    Builtin(builtin::Builtin),
    #[serde(rename = "definitions")]
    Inline(Vec<Definition>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[serde(untagged)]
pub enum Definition {
    Full {
        name: String,
        comment: Option<String>,
    },
    Inline(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct DefinitionFile {
    pub definitions: Vec<Definition>,
}

impl DefinitionFile {
    pub fn get_definitions(
        &self,
        path: &path::Path,
    ) -> HashMap<String, Vec<definitions::Definition>> {
        self.definitions
            .iter()
            .map(|def| {
                let (name, comment) = match def {
                    Definition::Full { name, comment } => (name.clone(), comment.clone()),
                    Definition::Inline(name) => (name.clone(), None),
                };
                (
                    name,
                    vec![definitions::Definition {
                        comment,
                        source: SourceInfo::Collection {
                            path: path.to_path_buf(),
                        },
                    }],
                )
            })
            .collect()
    }
}

impl EntryFile {
    pub async fn load_from_file(path: &path::Path) -> Result<Self, std::io::Error> {
        let content = tokio::fs::read_to_string(path).await?;
        let mut entry_file: EntryFile = toml::from_str(&content).map_err(|e| {
            error!("Failed to parse entry file: {}", e);
            std::io::ErrorKind::InvalidInput
        })?;
        entry_file.entry_point = path
            .parent()
            .unwrap()
            .join(entry_file.entry_point)
            .canonicalize()?;

        info!(
            "[Entry] Loaded: {:?}, Extensions: {:?}",
            path, entry_file.extension
        );

        Ok(entry_file)
    }

    pub async fn get_all_imports(
        &self,
        parser: &mut tree_sitter::Parser,
    ) -> Option<Vec<path::PathBuf>> {
        let content = tokio::fs::read_to_string(&self.entry_point).await.ok()?;
        let tree = parser.parse(&content, None)?;
        let q = tree_sitter::Query::new(
            &tree_sitter_lispbm::LANGUAGE.into(),
            r#"(special_form keyword: "import" (string "\"" (_)* @str "\"" ))"#,
        )
        .unwrap();
        let mut cursor = QueryCursor::new();
        let mut paths = vec![];
        let root = tree.root_node();
        cursor.matches(&q, root, content.as_bytes()).for_each(|m| {
            let node = m.captures[0].node;
            let path = node.utf8_text(content.as_bytes()).unwrap().to_string();

            if let Ok(p) = self.entry_point.parent().unwrap().join(path).canonicalize()
                && p.extension()
                    .and_then(|s| s.to_str())
                    .map(|s| s == "lisp" || s == "lispbm")
                    .unwrap_or(false)
            {
                paths.push(p);
            }
        });

        Some(paths)
    }

    pub fn get_ext_imports(&self) -> Result<Vec<path::PathBuf>, std::io::Error> {
        let mut paths = vec![];
        for ext in &self.extension {
            match ext {
                Extension::Collection(path_buf) => {
                    let path = self
                        .entry_point
                        .parent()
                        .ok_or(std::io::ErrorKind::IsADirectory)?
                        .join(path_buf)
                        .canonicalize()?;
                    info!("[Entry] Loaded extension collection: {:?}", path);
                    paths.push(path);
                }
                Extension::Inline(_) => {
                    let path = self.entry_point.clone();
                    paths.push(path);
                }
                Extension::Builtin(builtin) => {
                    let id = builtin.to_string();
                    info!("[Entry] Loaded extension builtin: {:?}", id);
                    paths.push(path::PathBuf::from(id));
                }
            }
        }

        Ok(paths)
    }

    pub fn get_ext_inline_definitions(&self) -> HashMap<String, Vec<definitions::Definition>> {
        self.extension
            .iter()
            .filter_map(|ext| match ext {
                Extension::Inline(definitions) => Some(definitions),
                _ => None,
            })
            .flatten()
            .map(|def| {
                let (name, comment) = match def {
                    Definition::Full { name, comment } => (name.clone(), comment.clone()),
                    Definition::Inline(name) => (name.clone(), None),
                };
                info!("[Enrty] Loaded inline definition: {}", name);
                (
                    name,
                    vec![definitions::Definition {
                        comment,
                        source: SourceInfo::Collection {
                            path: self.entry_point.clone(),
                        },
                    }],
                )
            })
            .collect()
    }

    pub async fn get_all_ext_definitions(&self) -> HashMap<String, Vec<definitions::Definition>> {
        let mut defs = HashMap::<String, Vec<definitions::Definition>>::new();
        for ext in &self.extension {
            match ext {
                Extension::Collection(path_buf) => {
                    let path = self
                        .entry_point
                        .parent()
                        .unwrap()
                        .join(path_buf)
                        .canonicalize()
                        .unwrap();
                    let content = match tokio::fs::read_to_string(&path).await {
                        Ok(c) => c,
                        Err(e) => {
                            error!(
                                "Failed to read definition collection file {:?}: {}",
                                path_buf, e
                            );
                            continue;
                        }
                    };
                    let def_file: DefinitionFile = toml::from_str(&content).unwrap();
                    info!("Loaded definitions from collection: {:?}", path_buf);
                    for def in def_file.definitions {
                        let (name, comment) = match def {
                            Definition::Full { name, comment } => (name, comment),
                            Definition::Inline(name) => (name, None),
                        };
                        let defs = defs.entry(name).or_default();
                        defs.push(definitions::Definition {
                            comment,
                            source: SourceInfo::Collection { path: path.clone() },
                        });
                    }
                }
                Extension::Builtin(builtin) => {
                    let def_file = builtin.get_def_file();
                    for def in def_file.definitions {
                        let (name, comment) = match def {
                            Definition::Full { name, comment } => (name, comment),
                            Definition::Inline(name) => (name, None),
                        };
                        let defs = defs.entry(name).or_default();
                        defs.push(definitions::Definition {
                            comment,
                            source: SourceInfo::Builtin {
                                name: builtin.clone(),
                            },
                        });
                    }
                }
                Extension::Inline(definitions) => {
                    for def in definitions {
                        let (name, comment) = match def {
                            Definition::Full { name, comment } => (name, comment),
                            Definition::Inline(name) => (name, &None),
                        };
                        let defs = defs.entry(name.clone()).or_default();
                        defs.push(definitions::Definition {
                            comment: comment.clone(),
                            source: SourceInfo::Collection {
                                path: self.entry_point.clone(),
                            },
                        });
                    }
                }
            }
        }

        defs
    }
}

mod tests {
    use super::*;

    #[test]
    fn test_entry_file_serialization() {
        let entry_str = r#"
            entry_point = "./main.lisp"

            [[extension]]
            path = "../../helpers.toml"

            [[extension]]
            builtin = "lbm"

            [[extension]]
            definitions = [
                {
                    name = "hello",
                    comment = """
                    A simple greeting function
                    """
                },
                {
                    name = "hello2",
                },
                "hello3"
            ]
        "#;
        let entry = EntryFile {
            entry_point: path::PathBuf::from("./main.lisp"),
            extension: vec![
                Extension::Collection(path::PathBuf::from("../../helpers.toml")),
                Extension::Builtin(builtin::Builtin::Lbm),
                Extension::Inline(vec![
                    Definition::Full {
                        name: "hello".to_string(),
                        comment: Some("A simple greeting function".to_string()),
                    },
                    Definition::Full {
                        name: "hello2".to_string(),
                        comment: None,
                    },
                    Definition::Inline("hello3".to_string()),
                ]),
            ],
        };

        let toml: EntryFile = toml::from_str(entry_str).unwrap();
        println!("Serialized EntryFile: {:#?}", toml);

        assert_eq!(toml.entry_point, entry.entry_point);
        assert_eq!(toml.extension.len(), entry.extension.len());
    }
}
