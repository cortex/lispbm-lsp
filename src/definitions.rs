use std::path;

use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct Definition {
    pub symbol: String,
    pub file: path::PathBuf,
    pub comment: Option<String>,
    pub line: usize,
    pub column: usize,
}
