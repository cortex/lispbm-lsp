use std::path;

use serde::{Deserialize, Serialize};
use tree_sitter::Node;

#[derive(Debug, Serialize, Deserialize)]
pub struct Definition {
    pub file: path::PathBuf,
    pub comment: Option<String>,
    pub line: usize,
    pub column: usize,
}

impl Definition {
    pub fn new(file: path::PathBuf, comment: Option<String>, line: usize, column: usize) -> Self {
        Self {
            file,
            comment,
            line,
            column,
        }
    }

    pub fn from_def_node(def_node: Node, file: &path::Path) -> Self {
        let comment = None;
        let line = def_node.start_position().row + 1;
        let column = def_node.start_position().column + 1;
        Self {
            file: file.to_path_buf(),
            comment,
            line,
            column,
        }
    }
}
