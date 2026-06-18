use std::{collections::HashMap, path};

use crate::{definitions::Definition, entry};

#[derive(Debug, Hash, Clone, PartialEq, Eq)]
pub struct EntryId(pub path::PathBuf);

impl From<path::PathBuf> for EntryId {
    fn from(value: path::PathBuf) -> Self {
        EntryId(value)
    }
}

impl From<String> for EntryId {
    fn from(value: String) -> Self {
        EntryId(path::PathBuf::from(value))
    }
}

#[derive(Debug, Default)]
pub struct State {
    pub entry_files: HashMap<EntryId, entry::EntryFile>,
    pub file_to_entry: HashMap<path::PathBuf, Vec<EntryId>>,
    pub root: path::PathBuf,
    pub definitions: HashMap<EntryId, HashMap<String, Definition>>,
}
