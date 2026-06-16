use std::{collections::HashMap, path};

use crate::{definitions::Definition, entry};

#[derive(Debug, Default)]
pub struct State {
    pub entry_files: HashMap<path::PathBuf, entry::EntryFile>,
    pub root: path::PathBuf,
    pub definitions: HashMap<path::PathBuf, HashMap<String, Definition>>,
}
