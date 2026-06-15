use std::path;

use serde::{Deserialize, Serialize};

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
