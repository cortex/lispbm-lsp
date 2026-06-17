use std::path;

use serde::{Deserialize, Serialize};
use tree_sitter::Node;

#[derive(Debug, Serialize, Deserialize)]
pub struct Definition {
    pub file: path::PathBuf,
    pub comment: Option<String>,
    pub line: u32,
    pub column: u32,
}

impl Definition {
    pub fn new(file: path::PathBuf, comment: Option<String>, line: u32, column: u32) -> Self {
        Self {
            file,
            comment,
            line,
            column,
        }
    }

    pub fn from_def_match(
        captures: &[tree_sitter::QueryCapture],
        content: &[u8],
        file: path::PathBuf,
    ) -> Result<(String, Self), String> {
        let mut name = None;
        let mut comment = None;

        let mut line: u32 = 0;
        let mut column: u32 = 0;

        for capture in captures.iter() {
            match capture.index {
                0 => {
                    // comment
                    let c = capture
                        .node
                        .utf8_text(content)
                        .map_err(|e| e.to_string())?
                        .trim_start_matches(';')
                        .trim()
                        .to_string();
                    if let Some(existing) = comment {
                        comment = Some(format!("{} {}", existing, c));
                    } else {
                        comment = Some(c);
                    }
                }
                1 if name.is_none() => {
                    // name
                    name = Some(
                        capture
                            .node
                            .utf8_text(content)
                            .map_err(|e| e.to_string())?
                            .to_string(),
                    );
                    let start = capture.node.start_position();
                    line = start.row as u32;
                    column = start.column as u32;
                }
                _ => return Err(format!("Unexpected capture index: {}", capture.index)),
            }
        }

        if name.is_none() {
            return Err("Definition match missing name capture".to_string());
        }

        Ok((
            name.unwrap(),
            Self {
                file,
                comment,
                line,
                column,
            },
        ))
    }
}
