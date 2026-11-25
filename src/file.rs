use std::{collections::HashMap, path, sync::LazyLock};

use tower_lsp_server::ls_types;
use tree_sitter::{Parser, Query, QueryCursor, StreamingIterator, Tree};

use crate::{
    builtin::Builtin,
    definitions::Definition,
    entry::{DefinitionFile, EntryFile},
};

static IMPORT_QUERY: LazyLock<Query> = LazyLock::new(|| {
    Query::new(
        &tree_sitter_lispbm::LANGUAGE.into(),
        r#"(special_form keyword: "import" (string "\"" (_)* @str "\"" ))"#,
    )
    .expect("valid import query")
});

static SYNTAX_ERROR_QUERY: LazyLock<Query> = LazyLock::new(|| {
    Query::new(
        &tree_sitter_lispbm::LANGUAGE.into(),
        "[(ERROR) @err (MISSING) @mis]",
    )
    .expect("valid syntax error query")
});

/// Identifies a source of definitions: a file on disk or one bundled with the server.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum FileId {
    Path(path::PathBuf),
    Builtin(Builtin),
}

impl FileId {
    /// Canonicalizes the path, so the same file reached through different paths gets the same id.
    /// Only the parent is canonicalized for files that don't exist.
    pub fn from_path(path: &path::Path) -> Self {
        let canonical = path.canonicalize().or_else(|e| {
            let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
                return Err(e);
            };
            Ok(parent.canonicalize()?.join(name))
        });
        FileId::Path(canonical.unwrap_or_else(|_| path.to_path_buf()))
    }

    pub async fn read(&self) -> std::io::Result<String> {
        match self {
            FileId::Path(path) => tokio::fs::read_to_string(path).await,
            FileId::Builtin(builtin) => Ok(builtin.get_ref().to_string()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    Lisp,
    Entry,
    Collection,
}

impl FileKind {
    /// Returns `None` for files the server doesn't understand.
    pub fn of(id: &FileId) -> Option<Self> {
        let path = match id {
            FileId::Path(path) => path,
            FileId::Builtin(_) => return Some(FileKind::Collection),
        };
        match path.extension()?.to_str()? {
            "lisp" | "lsp" | "lispbm" => Some(FileKind::Lisp),
            "toml" if path.file_name()? == "entry.toml" => Some(FileKind::Entry),
            "toml" => Some(FileKind::Collection),
            _ => None,
        }
    }
}

/// Everything known about a file. All fields describe the same version of `text`, except that
/// TOML files keep their last valid `definitions` and `entry` while `text` fails to parse.
#[derive(Debug, Default)]
pub struct File {
    pub text: String,
    pub tree: Option<Tree>,
    pub definitions: HashMap<String, Vec<Definition>>,
    /// Import paths exactly as written in the source.
    pub imports: Vec<String>,
    /// The parsed config, for entry files.
    pub entry: Option<EntryFile>,
    pub diagnostics: Vec<ls_types::Diagnostic>,
}

pub struct Symbol<'a> {
    pub name: &'a str,
    pub range: ls_types::Range,
}

impl File {
    /// Replaces the contents of the file. Returns true if the files it refers to have changed.
    pub fn update(&mut self, parser: &mut Parser, id: &FileId, text: String) -> bool {
        self.text = text;
        let changed = match FileKind::of(id) {
            Some(FileKind::Lisp) => Ok(self.update_lisp(parser, id)),
            Some(FileKind::Entry) => self.update_entry(id),
            Some(FileKind::Collection) => self.update_collection(id),
            None => Ok(false),
        };
        changed.unwrap_or_else(|e| {
            self.diagnostics = vec![toml_diagnostic(&self.text, &e)];
            false
        })
    }

    fn update_lisp(&mut self, parser: &mut Parser, id: &FileId) -> bool {
        let tree = parser
            .parse(&self.text, None)
            .expect("parser has a language and no timeout");

        self.definitions = match id {
            FileId::Path(path) => Definition::parse_definitions(&tree, path, self.text.as_bytes()),
            FileId::Builtin(_) => HashMap::new(),
        };
        self.diagnostics = syntax_errors(&tree, &self.text);
        let imports = parse_imports(&tree, &self.text);
        self.tree = Some(tree);

        let changed = self.imports != imports;
        self.imports = imports;
        changed
    }

    fn update_entry(&mut self, id: &FileId) -> Result<bool, toml::de::Error> {
        let entry: EntryFile = toml::from_str(&self.text)?;
        self.definitions = entry.inline_definitions(id);
        self.diagnostics.clear();

        let changed = self.entry.as_ref() != Some(&entry);
        self.entry = Some(entry);
        Ok(changed)
    }

    fn update_collection(&mut self, id: &FileId) -> Result<bool, toml::de::Error> {
        let collection: DefinitionFile = toml::from_str(&self.text)?;
        self.definitions = collection.definitions(id);
        self.diagnostics.clear();
        Ok(false)
    }

    pub fn symbol_at(&self, line: u32, column: u32) -> Option<Symbol<'_>> {
        let point = tree_sitter::Point::new(line as usize, column as usize);
        let node = self
            .tree
            .as_ref()?
            .root_node()
            .descendant_for_point_range(point, point)?;
        Some(Symbol {
            name: node.utf8_text(self.text.as_bytes()).ok()?,
            range: node_range(node),
        })
    }
}

fn parse_imports(tree: &Tree, text: &str) -> Vec<String> {
    let mut imports = vec![];
    QueryCursor::new()
        .matches(&IMPORT_QUERY, tree.root_node(), text.as_bytes())
        .for_each(|m| {
            if let Some(capture) = m.captures.first()
                && let Ok(path) = capture.node.utf8_text(text.as_bytes())
            {
                imports.push(path.to_string());
            }
        });
    imports
}

fn syntax_errors(tree: &Tree, text: &str) -> Vec<ls_types::Diagnostic> {
    let mut diagnostics = vec![];
    QueryCursor::new()
        .matches(&SYNTAX_ERROR_QUERY, tree.root_node(), text.as_bytes())
        .for_each(|m| {
            for capture in m.captures {
                let node = capture.node;
                let node_text = node.utf8_text(text.as_bytes()).unwrap_or("");
                diagnostics.push(error_diagnostic(
                    node_range(node),
                    format!("{}: {}", node.kind(), node_text),
                ));
            }
        });
    diagnostics
}

fn toml_diagnostic(text: &str, error: &toml::de::Error) -> ls_types::Diagnostic {
    let span = error.span().unwrap_or(0..0);
    let range = ls_types::Range {
        start: position_of(text, span.start),
        end: position_of(text, span.end),
    };
    error_diagnostic(range, error.message().to_string())
}

fn error_diagnostic(range: ls_types::Range, message: String) -> ls_types::Diagnostic {
    ls_types::Diagnostic {
        range,
        severity: Some(ls_types::DiagnosticSeverity::ERROR),
        message,
        ..Default::default()
    }
}

fn position_of(text: &str, offset: usize) -> ls_types::Position {
    let before = &text.as_bytes()[..offset.min(text.len())];
    let line_start = before
        .iter()
        .rposition(|b| *b == b'\n')
        .map_or(0, |i| i + 1);
    ls_types::Position {
        line: before.iter().filter(|b| **b == b'\n').count() as u32,
        character: (before.len() - line_start) as u32,
    }
}

fn node_range(node: tree_sitter::Node) -> ls_types::Range {
    let position = |point: tree_sitter::Point| ls_types::Position {
        line: point.row as u32,
        character: point.column as u32,
    };
    ls_types::Range {
        start: position(node.start_position()),
        end: position(node.end_position()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parser() -> Parser {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_lispbm::LANGUAGE.into())
            .unwrap();
        parser
    }

    fn id(path: &str) -> FileId {
        FileId::Path(path.into())
    }

    #[test]
    fn kinds() {
        for path in ["/a.lisp", "/a.lsp", "/a.lispbm"] {
            assert_eq!(FileKind::of(&id(path)), Some(FileKind::Lisp));
        }
        assert_eq!(FileKind::of(&id("/x/entry.toml")), Some(FileKind::Entry));
        assert_eq!(FileKind::of(&id("/x/e.toml")), Some(FileKind::Collection));
        assert_eq!(FileKind::of(&id("/a.bin")), None);
        assert_eq!(FileKind::of(&id("/a")), None);
    }

    #[test]
    fn update_replaces_every_field() {
        let (mut parser, mut file, id) = (parser(), File::default(), id("/main.lisp"));
        assert!(file.update(
            &mut parser,
            &id,
            "(import \"a.lisp\" 'a)\n(def old 1)\n".into()
        ));
        assert_eq!(file.imports, ["a.lisp"]);

        // Shrinking the text must not leave offsets from the old version behind.
        assert!(file.update(&mut parser, &id, "(def new 1)".into()));
        assert!(file.imports.is_empty());
        assert!(!file.definitions.contains_key("old"));
        assert!(file.definitions.contains_key("new"));
        assert_eq!(file.symbol_at(0, 6).unwrap().name, "new");
        assert!(file.diagnostics.is_empty());

        assert!(!file.update(&mut parser, &id, "(def new".into()));
        assert!(!file.diagnostics.is_empty());
    }

    #[test]
    fn invalid_toml_is_reported_and_keeps_definitions() {
        let (mut parser, mut file, id) = (parser(), File::default(), id("/ext.toml"));
        file.update(&mut parser, &id, "definitions = [\"a\"]".into());
        assert!(file.diagnostics.is_empty());

        file.update(&mut parser, &id, "definitions = [\"a\"]\n[[ext".into());
        assert_eq!(file.diagnostics.len(), 1);
        assert_eq!(file.diagnostics[0].range.start.line, 1);
        assert!(file.definitions.contains_key("a"));

        file.update(&mut parser, &id, "definitions = [\"b\"]".into());
        assert!(file.diagnostics.is_empty());
        assert!(!file.definitions.contains_key("a"));
    }
}
