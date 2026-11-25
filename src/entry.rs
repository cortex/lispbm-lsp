use std::{collections::HashMap, path};

use serde::{Deserialize, Serialize};

use crate::{builtin, definitions, file::FileId};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntryFile {
    pub entry_point: path::PathBuf,
    pub extension: Vec<Extension>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Extension {
    #[serde(rename = "path")]
    Collection(path::PathBuf),
    Builtin(builtin::Builtin),
    #[serde(rename = "definitions")]
    Inline(Vec<Definition>),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[serde(untagged)]
pub enum Definition {
    Full {
        name: String,
        comment: Option<String>,
    },
    Inline(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct DefinitionFile {
    pub definitions: Vec<Definition>,
}

impl DefinitionFile {
    pub fn definitions(&self, id: &FileId) -> HashMap<String, Vec<definitions::Definition>> {
        collect_definitions(&self.definitions, id)
    }
}

impl EntryFile {
    /// `dir` is the directory containing the entry file, which all its paths are relative to.
    pub fn entry_point(&self, dir: &path::Path) -> FileId {
        FileId::from_path(&dir.join(&self.entry_point))
    }

    /// Files providing extensions. See [`EntryFile::entry_point`] for `dir`.
    pub fn extension_files(&self, dir: &path::Path) -> Vec<FileId> {
        self.extension
            .iter()
            .filter_map(|ext| match ext {
                Extension::Collection(path) => Some(FileId::from_path(&dir.join(path))),
                Extension::Builtin(builtin) => Some(FileId::Builtin(*builtin)),
                Extension::Inline(_) => None,
            })
            .collect()
    }

    pub fn inline_definitions(&self, id: &FileId) -> HashMap<String, Vec<definitions::Definition>> {
        let inline = self.extension.iter().filter_map(|ext| match ext {
            Extension::Inline(definitions) => Some(definitions),
            _ => None,
        });
        collect_definitions(inline.flatten(), id)
    }
}

fn collect_definitions<'a>(
    definitions: impl IntoIterator<Item = &'a Definition>,
    id: &FileId,
) -> HashMap<String, Vec<definitions::Definition>> {
    let mut collected = HashMap::<_, Vec<_>>::new();
    for def in definitions {
        let (name, comment) = match def {
            Definition::Full { name, comment } => (name, comment.clone()),
            Definition::Inline(name) => (name, None),
        };
        collected
            .entry(name.clone())
            .or_default()
            .push(definitions::Definition {
                comment,
                source: id.into(),
            });
    }
    collected
}

#[cfg(test)]
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

    #[test]
    fn paths_are_relative_to_the_entry_file() {
        let entry: EntryFile = toml::from_str(
            "entry_point = \"src/main.lisp\"\nextension = [{ path = \"ext.toml\" }, { builtin = \"lbm\" }]",
        )
        .unwrap();
        let dir = path::Path::new("/does-not-exist/project");

        assert_eq!(
            entry.entry_point(dir),
            FileId::Path(dir.join("src/main.lisp"))
        );
        assert_eq!(
            entry.extension_files(dir),
            [
                FileId::Path(dir.join("ext.toml")),
                FileId::Builtin(builtin::Builtin::Lbm)
            ]
        );
    }

    #[test]
    fn repeated_names_are_all_kept() {
        let file: DefinitionFile = toml::from_str("definitions = [\"a\", \"b\", \"a\"]").unwrap();
        let defs = file.definitions(&FileId::Builtin(builtin::Builtin::Lbm));
        assert_eq!(defs["a"].len(), 2);
        assert_eq!(defs["b"].len(), 1);
    }
}
