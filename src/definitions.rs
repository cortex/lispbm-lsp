use std::{collections::HashMap, path};

use serde::{Deserialize, Serialize};
use tree_sitter::{QueryCursor, StreamingIterator};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Definition {
    pub file: path::PathBuf,
    pub comment: Option<String>,
    pub line: u32,
    pub column: u32,
    pub len: u32,
}

impl Definition {
    pub fn parse_definitions(
        tree: &tree_sitter::Tree,
        path: &path::Path,
        content: &[u8],
    ) -> Result<HashMap<String, Vec<Self>>, String> {
        let q = tree_sitter::Query::new(
            &tree_sitter_lispbm::LANGUAGE.into(),
            r#"
            (_
                (comment)* @doc_comment
                .
                [
                  (definition name: (symbol) @name ) @node
                  (function_definition name: (symbol) @name ) @node
                ]
                .
                (comment)* @doc_comment)
            "#,
        )
        .unwrap();

        let mut cursor = QueryCursor::new();
        let root = tree.root_node();
        let mut defs = HashMap::<String, Vec<Self>>::new();
        cursor.matches(&q, root, content).for_each(|m| {
            let (name, def) = Self::from_def_match(m.captures, content, path.into()).unwrap();
            defs.entry(name).or_default().push(def);
        });

        Ok(defs)
    }

    pub fn from_def_match(
        captures: &[tree_sitter::QueryCapture],
        content: &[u8],
        file: path::PathBuf,
    ) -> Result<(String, Self), String> {
        let node = captures
            .iter()
            .find(|c| c.index == 2)
            .ok_or("Definition match missing name capture")?
            .node;
        let node_start_line = node.start_position().row as u32;
        let node_end_line = node.end_position().row as u32;

        let name_node = captures
            .iter()
            .find(|c| c.index == 1)
            .ok_or("Definition match missing name capture".to_string())?;

        let line = name_node.node.start_position().row as u32;
        let column = name_node.node.start_position().column as u32;
        let len = name_node.node.end_position().column as u32 - column;

        let name = name_node
            .node
            .utf8_text(content)
            .map_err(|e| e.to_string())
            .map(|s| s.to_string())?;

        let mut comment = None;

        let mut all_nodes_above_definition = captures
            .iter()
            .filter(|c| c.index == 0 && c.node.start_position().row < node_start_line as usize)
            .map(|c| c.node)
            .collect::<Vec<_>>();
        all_nodes_above_definition.sort_by_key(|n| n.start_position().row);
        all_nodes_above_definition.reverse();

        // Check for comments above the definition, by line number
        let mut current_line = node_start_line;
        for comment_node in all_nodes_above_definition {
            if comment_node.start_position().row as u32 == current_line - 1 {
                let c = comment_node
                    .utf8_text(content)
                    .map_err(|e| e.to_string())?
                    .trim_start_matches(';')
                    .trim()
                    .to_string();
                if !c.is_empty() {
                    if let Some(existing) = comment {
                        comment = Some(format!("{} {}", c, existing));
                    } else {
                        comment = Some(c);
                    }
                }
                current_line -= 1;
            } else {
                break;
            }
        }

        let comment_node_at_end_line = captures
            .iter()
            .find(|c| c.index == 0 && c.node.start_position().row as u32 == node_end_line)
            .map(|c| c.node);

        if let Some(comment_node) = comment_node_at_end_line {
            let c = comment_node
                .utf8_text(content)
                .map_err(|e| e.to_string())?
                .trim_start_matches(';')
                .trim()
                .to_string();
            if !c.is_empty() {
                if let Some(existing) = comment {
                    // if the comment is on the same line as the definition, only use the comment after the definition
                    if comment_node.start_position().row as u32 == line {
                        comment = Some(c);
                    } else {
                        comment = Some(format!("{} {}", existing, c));
                    }
                } else {
                    comment = Some(c);
                }
            }
        }

        Ok((
            name,
            Self {
                file,
                comment,
                line,
                column,
                len,
            },
        ))
    }
}
