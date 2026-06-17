use std::{collections::HashMap, path};

use serde::{Deserialize, Serialize};
use tower_lsp_server::{Client, ls_types::MessageType};
use tree_sitter::{QueryCursor, StreamingIterator};

use crate::definitions::Definition;

#[derive(Debug, Serialize, Deserialize)]
pub struct EntryFile {
    pub entry_point: path::PathBuf,
    pub extension: Vec<Extension>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Extension {
    Path(path::PathBuf),
    Builtin(String),
    Definition(Vec<String>),
}

impl EntryFile {
    pub fn find_closest_entry_file(path: &path::Path, root: &path::Path) -> Option<path::PathBuf> {
        let mut current = path.strip_prefix(root).ok()?;
        const MAX_DEPTH: usize = 10;
        for _ in 0..MAX_DEPTH {
            let entry_path = current.join("entry.toml");
            if entry_path.exists() {
                return Some(entry_path);
            }
            if let Some(parent) = current.parent() {
                current = parent;
            } else {
                break;
            }
        }
        None
    }

    pub fn load_from_file(path: &path::Path) -> Result<Self, std::io::Error> {
        let content = std::fs::read_to_string(path)?;
        let mut entry_file: EntryFile =
            toml::from_str(&content).map_err(|_| std::io::ErrorKind::InvalidInput)?;
        entry_file.entry_point = path
            .parent()
            .unwrap()
            .join(entry_file.entry_point)
            .canonicalize()?;

        Ok(entry_file)
    }

    pub async fn get_all_imports(
        &self,
        parser: &mut tree_sitter::Parser,
    ) -> Option<Vec<path::PathBuf>> {
        let content = std::fs::read_to_string(&self.entry_point).ok()?;
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
            builtin = "core"

            [[extension]]
            definition = [
                "hello"
            ]
        "#;
        let entry = EntryFile {
            entry_point: path::PathBuf::from("./main.lisp"),
            extension: vec![
                Extension::Path(path::PathBuf::from("../../helpers.toml")),
                Extension::Builtin("core".to_string()),
                Extension::Definition(vec!["foo".to_string(), "bar".to_string()]),
            ],
        };

        let toml: EntryFile = toml::from_str(entry_str).unwrap();
        println!("Serialized EntryFile: {:#?}", toml);

        assert_eq!(toml.entry_point, entry.entry_point);
        assert_eq!(toml.extension.len(), entry.extension.len());
    }
}
