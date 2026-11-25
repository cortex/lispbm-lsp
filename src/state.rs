use std::{
    collections::{BTreeMap, HashMap},
    path,
};

use tokio::sync::{mpsc, oneshot};
use tower_lsp_server::ls_types;
use tracing::{error, info, warn};

use crate::{
    definitions::{Definition, SourceInfo},
    file::{File, FileId, FileKind},
};

#[derive(Debug)]
pub enum Request {
    OpenWorkspace {
        root: path::PathBuf,
    },
    /// A document was opened or edited.
    DocumentUpdated {
        file: FileId,
        text: String,
        response: oneshot::Sender<Vec<ls_types::Diagnostic>>,
    },
    DocumentSaved {
        file: FileId,
    },
    DocumentClosed {
        file: FileId,
    },
    GetDefinition {
        file: FileId,
        position: ls_types::Position,
        response: oneshot::Sender<Vec<ls_types::Location>>,
    },
    GetHover {
        file: FileId,
        position: ls_types::Position,
        response: oneshot::Sender<Option<ls_types::Hover>>,
    },
}

pub struct State {
    parser: tree_sitter::Parser,
    roots: Vec<path::PathBuf>,
    /// Text of the documents open in the editor, which takes priority over what is on disk.
    open: HashMap<FileId, String>,
    /// The files belonging to each entry file, in the order definitions are listed. May contain
    /// files that could not be loaded, so they are picked up once they exist.
    entries: BTreeMap<FileId, Vec<FileId>>,
    /// Loaded files. Always a subset of the files in `entries`.
    files: HashMap<FileId, File>,
}

impl State {
    pub fn new() -> Self {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_lispbm::LANGUAGE.into())
            .expect("Error loading lispBM grammar");

        State {
            parser,
            roots: vec![],
            open: HashMap::new(),
            entries: BTreeMap::new(),
            files: HashMap::new(),
        }
    }

    pub async fn run(mut self, mut rx: mpsc::Receiver<Request>) {
        while let Some(request) = rx.recv().await {
            self.handle(request).await;
        }
    }

    /// Responses are dropped if the client is no longer waiting for them.
    async fn handle(&mut self, request: Request) {
        match request {
            Request::OpenWorkspace { root } => self.open_workspace(root).await,
            Request::DocumentUpdated {
                file,
                text,
                response,
            } => {
                let _ = response.send(self.update_document(file, text).await);
            }
            Request::DocumentSaved { file } => self.document_saved(file).await,
            Request::DocumentClosed { file } => self.close_document(file).await,
            Request::GetDefinition {
                file,
                position,
                response,
            } => {
                let _ = response.send(self.goto(&file, position));
            }
            Request::GetHover {
                file,
                position,
                response,
            } => {
                let _ = response.send(self.hover(&file, position));
            }
        }
    }

    async fn open_workspace(&mut self, root: path::PathBuf) {
        info!("[Workspace] Opening {:?}", root);
        let pattern =
            path::Path::new(&glob::Pattern::escape(&root.to_string_lossy())).join("**/entry.toml");
        // The same canonical form as file ids, so hover can show paths relative to the root.
        self.roots.push(root.canonicalize().unwrap_or(root));

        let matches = match glob::glob(&pattern.to_string_lossy()) {
            Ok(matches) => matches,
            Err(e) => {
                error!(
                    "[Workspace] Invalid entry file pattern {:?}: {}",
                    pattern, e
                );
                return;
            }
        };
        for entry in matches {
            match entry {
                Ok(path) => self.add_entry(FileId::from_path(&path)).await,
                Err(e) => error!("[Workspace] Error finding entry files: {}", e),
            }
        }
    }

    async fn add_entry(&mut self, id: FileId) {
        self.entries.insert(id.clone(), vec![id.clone()]);
        self.ensure_loaded(&id).await;
        self.resolve_entry(&id).await;

        if let Some(entry_point) = self.entry_point(&id)
            && !self.files.contains_key(&entry_point)
        {
            error!("[Entry] {:?}: could not load {:?}", id, entry_point);
        }
    }

    fn entry_point(&self, id: &FileId) -> Option<FileId> {
        let FileId::Path(path) = id else { return None };
        let config = self.files.get(id)?.entry.as_ref()?;
        Some(config.entry_point(path.parent()?))
    }

    /// Works out which files belong to an entry, and loads the ones that are missing.
    async fn resolve_entry(&mut self, id: &FileId) {
        let Some(entry_point) = self.entry_point(id) else {
            return;
        };
        self.ensure_loaded(&entry_point).await;

        let mut members = vec![entry_point.clone()];
        let mut add = |file: FileId| {
            if !members.contains(&file) {
                members.push(file);
            }
        };
        if let FileId::Path(path) = &entry_point
            && let Some(dir) = path.parent()
            && let Some(file) = self.files.get(&entry_point)
        {
            file.imports
                .iter()
                .map(|import| FileId::from_path(&dir.join(import)))
                .filter(|import| FileKind::of(import) == Some(FileKind::Lisp))
                .for_each(&mut add);
        }
        add(id.clone());
        if let FileId::Path(path) = id
            && let Some(dir) = path.parent()
            && let Some(config) = self.files.get(id).and_then(|file| file.entry.as_ref())
        {
            config.extension_files(dir).into_iter().for_each(&mut add);
        }

        for member in &members {
            self.ensure_loaded(member).await;
        }
        info!("[Entry] {:?} has files {:?}", id, members);
        self.entries.insert(id.clone(), members);
        self.drop_unused_files();
    }

    async fn resolve_entries_with(&mut self, file: &FileId) {
        let affected: Vec<_> = self
            .entries
            .iter()
            .filter(|(_, members)| members.contains(file))
            .map(|(id, _)| id.clone())
            .collect();
        for id in affected {
            self.resolve_entry(&id).await;
        }
    }

    fn drop_unused_files(&mut self) {
        let entries = &self.entries;
        self.files
            .retain(|id, _| entries.values().any(|members| members.contains(id)));
    }

    fn belongs_to_entry(&self, file: &FileId) -> bool {
        self.entries.values().any(|members| members.contains(file))
    }

    async fn ensure_loaded(&mut self, id: &FileId) {
        if self.files.contains_key(id) {
            return;
        }
        if FileKind::of(id).is_none() {
            warn!("[File] Unsupported file type {:?}", id);
            return;
        }
        let text = match self.open.get(id) {
            Some(text) => text.clone(),
            None => match id.read().await {
                Ok(text) => text,
                Err(e) => {
                    warn!("[File] Failed to read {:?}: {}", id, e);
                    return;
                }
            },
        };

        let mut file = File::default();
        file.update(&mut self.parser, id, text);
        // Nothing can be provided from a broken TOML file, and it might not be open in the editor.
        if FileKind::of(id) != Some(FileKind::Lisp) {
            for diagnostic in &file.diagnostics {
                error!("[File] Failed to parse {:?}: {}", id, diagnostic.message);
            }
        }
        info!(
            "[File] Loaded {:?} with {} definitions",
            id,
            file.definitions.len()
        );
        self.files.insert(id.clone(), file);
    }

    async fn update_document(&mut self, id: FileId, text: String) -> Vec<ls_types::Diagnostic> {
        self.open.insert(id.clone(), text.clone());
        if FileKind::of(&id).is_none() || !self.belongs_to_entry(&id) {
            return vec![];
        }

        let file = self.files.entry(id.clone()).or_default();
        let references_changed = file.update(&mut self.parser, &id, text);
        let diagnostics = file.diagnostics.clone();
        if references_changed {
            self.resolve_entries_with(&id).await;
        }
        diagnostics
    }

    /// The contents were already received as changes, but the file might not have been loadable
    /// before.
    async fn document_saved(&mut self, id: FileId) {
        if self.belongs_to_entry(&id) && !self.files.contains_key(&id) {
            self.ensure_loaded(&id).await;
            self.resolve_entries_with(&id).await;
        }
    }

    /// Unsaved changes are discarded, so go back to what is on disk.
    async fn close_document(&mut self, id: FileId) {
        self.open.remove(&id);
        if !self.belongs_to_entry(&id) {
            return;
        }

        self.files.remove(&id);
        self.ensure_loaded(&id).await;
        if !self.files.contains_key(&id) && self.entries.remove(&id).is_some() {
            info!("[Entry] Removed {:?}", id);
            self.drop_unused_files();
        }
        self.resolve_entries_with(&id).await;
    }

    /// The definitions visible from a file: those in files sharing an entry with it.
    fn definitions_of(&self, file: &FileId, symbol: &str) -> Vec<&Definition> {
        let mut visible: Vec<&FileId> = vec![];
        for members in self.entries.values().filter(|m| m.contains(file)) {
            for member in members {
                if !visible.contains(&member) {
                    visible.push(member);
                }
            }
        }
        visible
            .into_iter()
            .filter_map(|id| self.files.get(id)?.definitions.get(symbol))
            .flatten()
            .collect()
    }

    /// The range of the symbol at a position, and its definitions.
    fn lookup(
        &self,
        id: &FileId,
        position: ls_types::Position,
    ) -> Option<(ls_types::Range, Vec<&Definition>)> {
        let symbol = self
            .files
            .get(id)?
            .symbol_at(position.line, position.character)?;
        Some((symbol.range, self.definitions_of(id, symbol.name)))
    }

    fn goto(&self, id: &FileId, position: ls_types::Position) -> Vec<ls_types::Location> {
        let Some((_, definitions)) = self.lookup(id, position) else {
            return vec![];
        };
        definitions.into_iter().filter_map(location).collect()
    }

    fn hover(&self, id: &FileId, position: ls_types::Position) -> Option<ls_types::Hover> {
        let (range, definitions) = self.lookup(id, position)?;
        if definitions.is_empty() {
            return None;
        }
        let value = definitions
            .into_iter()
            .map(|def| {
                let comment = def.comment.as_deref().unwrap_or_default();
                format!("{}\n\n__{}__", comment, self.source_label(&def.source))
            })
            .collect::<Vec<_>>()
            .join("\n\n---\n\n");
        Some(ls_types::Hover {
            contents: ls_types::HoverContents::Markup(ls_types::MarkupContent {
                kind: ls_types::MarkupKind::Markdown,
                value,
            }),
            range: Some(range),
        })
    }

    fn source_label(&self, source: &SourceInfo) -> String {
        match source {
            SourceInfo::Source { file, .. } => file
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_else(|| file.display().to_string()),
            SourceInfo::Builtin { name } => name.to_string(),
            SourceInfo::Collection { path } => self
                .roots
                .iter()
                .find(|root| path.starts_with(root))
                .or(self.roots.last())
                .and_then(|root| pathdiff::diff_paths(path, root))
                .unwrap_or_else(|| path.clone())
                .display()
                .to_string(),
        }
    }
}

/// Only definitions from source code have a location to go to.
fn location(definition: &Definition) -> Option<ls_types::Location> {
    let SourceInfo::Source {
        file,
        line,
        column,
        len,
    } = &definition.source
    else {
        return None;
    };
    Some(ls_types::Location {
        uri: ls_types::Uri::from_file_path(file)?,
        range: ls_types::Range {
            start: ls_types::Position {
                line: *line,
                character: *column,
            },
            end: ls_types::Position {
                line: *line,
                character: column + len,
            },
        },
    })
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        sync::atomic::{AtomicUsize, Ordering},
    };

    use super::*;

    /// A workspace in a temporary directory, removed when dropped.
    struct Workspace {
        root: path::PathBuf,
        state: State,
    }

    impl Workspace {
        async fn new(files: &[(&str, &str)]) -> Self {
            static COUNT: AtomicUsize = AtomicUsize::new(0);
            let root = std::env::temp_dir().join(format!(
                "lispbm-lsp-test-{}-{}",
                std::process::id(),
                COUNT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&root).unwrap();
            let mut workspace = Workspace {
                root,
                state: State::new(),
            };
            for (path, text) in files {
                workspace.write(path, text);
            }
            let root = workspace.root.clone();
            workspace.state.open_workspace(root).await;
            workspace
        }

        fn write(&self, path: &str, text: &str) {
            let path = self.root.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }

        fn id(&self, path: &str) -> FileId {
            FileId::from_path(&self.root.join(path))
        }

        async fn edit(&mut self, path: &str, text: &str) -> Vec<ls_types::Diagnostic> {
            let id = self.id(path);
            self.state.update_document(id, text.to_string()).await
        }

        async fn close(&mut self, path: &str) {
            let id = self.id(path);
            self.state.close_document(id).await;
        }

        /// Comments of the definitions of `symbol` visible from `path`.
        fn comments(&self, path: &str, symbol: &str) -> Vec<String> {
            self.state
                .definitions_of(&self.id(path), symbol)
                .into_iter()
                .map(|def| def.comment.clone().unwrap_or_default())
                .collect()
        }
    }

    impl Drop for Workspace {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    const ENTRY: &str = "entry_point = \"main.lisp\"\nextension = []";

    #[tokio::test]
    async fn lookup_is_scoped_to_the_entry() {
        let ws = Workspace::new(&[
            ("a/entry.toml", ENTRY),
            ("a/main.lisp", "(def x 1) ; from a"),
            ("b/entry.toml", ENTRY),
            ("b/main.lisp", "(def x 1) ; from b"),
        ])
        .await;
        assert_eq!(ws.comments("a/main.lisp", "x"), ["from a"]);
        assert_eq!(ws.comments("b/main.lisp", "x"), ["from b"]);
    }

    #[tokio::test]
    async fn file_shared_by_two_entries_sees_both() {
        let main = |from| format!("(import \"../lib.lisp\" 'l)\n(def x 1) ; from {from}");
        let ws = Workspace::new(&[
            ("a/entry.toml", ENTRY),
            ("a/main.lisp", &main("a")),
            ("b/entry.toml", ENTRY),
            ("b/main.lisp", &main("b")),
            ("lib.lisp", "(def lib 1) ; lib"),
        ])
        .await;
        assert_eq!(ws.comments("lib.lisp", "x"), ["from a", "from b"]);
        assert_eq!(ws.comments("lib.lisp", "lib"), ["lib"]);
        assert_eq!(ws.comments("a/main.lisp", "x"), ["from a"]);
    }

    #[tokio::test]
    async fn edits_replace_text_and_definitions() {
        let mut ws = Workspace::new(&[
            ("entry.toml", ENTRY),
            ("main.lisp", "(def long-old-name 1)\n(long-old-name)"),
        ])
        .await;
        ws.edit("main.lisp", "(def a 1)\n(a)").await;
        assert!(ws.comments("main.lisp", "long-old-name").is_empty());

        // Positions after the end of the previous text used to panic.
        let text = format!("(def a 1) ; doc\n{}(a)", "; pad\n".repeat(100));
        ws.edit("main.lisp", &text).await;
        let position = ls_types::Position {
            line: 101,
            character: 1,
        };
        let id = ws.id("main.lisp");
        assert!(ws.state.hover(&id, position).is_some());
        assert_eq!(ws.state.goto(&id, position).len(), 1);
        let past_end = ls_types::Position {
            line: 999,
            character: 9,
        };
        assert!(ws.state.hover(&id, past_end).is_none());
    }

    #[tokio::test]
    async fn import_edits_load_and_drop_files() {
        let mut ws = Workspace::new(&[
            ("entry.toml", ENTRY),
            ("main.lisp", "(import \"a.lisp\" 'a)"),
            ("a.lisp", "(def a 1) ; a"),
            ("b.lispbm", "(def b 1) ; b"),
        ])
        .await;
        assert_eq!(ws.comments("main.lisp", "a"), ["a"]);

        // Uses the open buffer rather than what is on disk.
        ws.edit("b.lispbm", "(def b 1) ; unsaved").await;
        ws.edit("main.lisp", "(import \"b.lispbm\" 'b)").await;
        assert!(ws.comments("main.lisp", "a").is_empty());
        assert_eq!(ws.comments("main.lisp", "b"), ["unsaved"]);
        assert!(!ws.state.files.contains_key(&ws.id("a.lisp")));
    }

    #[tokio::test]
    async fn imported_file_created_later_is_loaded() {
        let mut ws = Workspace::new(&[
            ("entry.toml", ENTRY),
            ("main.lisp", "(import \"new.lisp\" 'n)"),
        ])
        .await;
        ws.edit("new.lisp", "(def n 1) ; new").await;
        assert_eq!(ws.comments("main.lisp", "n"), ["new"]);
    }

    #[tokio::test]
    async fn file_created_elsewhere_is_loaded_on_save() {
        let mut ws = Workspace::new(&[
            ("entry.toml", ENTRY),
            ("main.lisp", "(import \"new.lisp\" 'n)"),
        ])
        .await;
        ws.write("new.lisp", "(def n 1) ; new");
        let id = ws.id("new.lisp");
        ws.state.document_saved(id).await;
        assert_eq!(ws.comments("main.lisp", "n"), ["new"]);

        // Saving a file that is already loaded changes nothing.
        let id = ws.id("main.lisp");
        ws.state.document_saved(id).await;
        assert_eq!(ws.comments("main.lisp", "n"), ["new"]);
    }

    #[tokio::test]
    async fn extensions_resolve_from_the_entry_file() {
        let ws = Workspace::new(&[
            (
                "entry.toml",
                "entry_point = \"src/main.lisp\"\n\
                 extension = [{ path = \"missing.toml\" }, { path = \"ext.txt\" }, \
                 { path = \"ext.toml\" }, { builtin = \"array\" }, { definitions = [\"inl\"] }]",
            ),
            ("src/main.lisp", ""),
            (
                "ext.toml",
                "definitions = [{ name = \"e\", comment = \"ext\" }]",
            ),
            ("ext.txt", ""),
        ])
        .await;
        // Extensions that can't be loaded don't affect the others.
        assert_eq!(ws.comments("src/main.lisp", "e"), ["ext"]);
        assert_eq!(ws.comments("src/main.lisp", "inl").len(), 1);

        let builtin = ws.state.definitions_of(&ws.id("src/main.lisp"), "buflen");
        assert_eq!(
            ws.state.source_label(&builtin[0].source),
            "array.builtin.ext.toml"
        );
        let inline = ws.state.definitions_of(&ws.id("src/main.lisp"), "inl");
        assert_eq!(ws.state.source_label(&inline[0].source), "entry.toml");
    }

    #[tokio::test]
    async fn entry_edits_apply_once_valid() {
        let mut ws = Workspace::new(&[
            ("entry.toml", ENTRY),
            ("main.lisp", "(def m 1) ; main"),
            ("other.lisp", "(def o 1) ; other"),
        ])
        .await;
        let broken = ws.edit("entry.toml", "entry_point = ").await;
        assert_eq!(broken.len(), 1);
        assert_eq!(ws.comments("main.lisp", "m"), ["main"]);

        let other = "entry_point = \"other.lisp\"\nextension = []";
        assert!(ws.edit("entry.toml", other).await.is_empty());
        assert_eq!(ws.comments("other.lisp", "o"), ["other"]);
        assert!(ws.comments("other.lisp", "m").is_empty());
    }

    #[tokio::test]
    async fn closing_discards_unsaved_changes() {
        let mut ws = Workspace::new(&[
            ("entry.toml", ENTRY),
            ("main.lisp", "(import \"a.lisp\" 'a)\n(def saved 1) ; saved"),
            ("a.lisp", "(def a 1) ; a"),
        ])
        .await;
        ws.edit("main.lisp", "(def unsaved 1)").await;
        ws.close("main.lisp").await;
        assert!(ws.comments("main.lisp", "unsaved").is_empty());
        assert_eq!(ws.comments("main.lisp", "saved"), ["saved"]);
        assert_eq!(ws.comments("main.lisp", "a"), ["a"]);

        fs::remove_file(ws.root.join("a.lisp")).unwrap();
        ws.close("a.lisp").await;
        assert!(ws.comments("main.lisp", "a").is_empty());

        fs::remove_file(ws.root.join("entry.toml")).unwrap();
        ws.close("entry.toml").await;
        assert!(ws.state.entries.is_empty());
        assert!(ws.state.files.is_empty());
    }

    #[tokio::test]
    async fn files_outside_entries_are_ignored() {
        let mut ws = Workspace::new(&[("entry.toml", ENTRY), ("main.lisp", "")]).await;
        assert!(ws.edit("loose.lisp", "(def a").await.is_empty());
        assert!(!ws.state.files.contains_key(&ws.id("loose.lisp")));
        assert!(!ws.edit("main.lisp", "(def a").await.is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlinked_paths_are_the_same_file() {
        let ws = Workspace::new(&[
            ("real/entry.toml", ENTRY),
            ("real/main.lisp", "(def m 1) ; main"),
        ])
        .await;
        std::os::unix::fs::symlink(ws.root.join("real"), ws.root.join("link")).unwrap();
        assert_eq!(ws.comments("link/main.lisp", "m"), ["main"]);
    }
}
