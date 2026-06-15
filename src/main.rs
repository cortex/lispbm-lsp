use tower_lsp_server::ls_types::*;
use tree_sitter::Node;

fn collect_syntax_errors(node: Node, diagnostics: &mut Vec<Diagnostic>) {
    if node.is_error() || node.is_missing() {
        let range = Range {
            start: Position::new(
                node.start_position().row as u32,
                node.start_position().column as u32,
            ),
            end: Position::new(
                node.end_position().row as u32,
                node.end_position().column as u32,
            ),
        };

        diagnostics.push(Diagnostic {
            range,
            severity: Some(DiagnosticSeverity::ERROR),
            message: format!("Syntax error: unexpected {}", node.kind()),
            ..Default::default()
        });
    }

    // Recursively check children
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_syntax_errors(child, diagnostics);
    }
}

use tower_lsp_server::{Client, LanguageServer};

struct Backend {
    client: Client,
    parser: Mutex<tree_sitter::Parser>,
}

use tower_lsp_server::jsonrpc::Result;

impl LanguageServer for Backend {
    async fn initialize(&self, _: InitializeParams) -> Result<InitializeResult> {
        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::FULL,
                )),
                ..Default::default()
            },
            ..Default::default()
        })
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        self.on_change(params.text_document.uri, params.text_document.text)
            .await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        // For simplicity, we assume TextDocumentSyncKind::FULL
        if let Some(event) = params.content_changes.first() {
            self.on_change(params.text_document.uri, event.text.clone())
                .await;
        }
    }
    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }
}

impl Backend {
    async fn on_change(&self, uri: Uri, text: String) {
        let mut parser = self.parser.lock().await;
        let tree = parser.parse(&text, None).unwrap();

        let mut diagnostics = Vec::new();
        collect_syntax_errors(tree.root_node(), &mut diagnostics);

        self.client
            .publish_diagnostics(uri, diagnostics, None)
            .await;
    }
}

use tokio::sync::Mutex;
use tower_lsp_server::{LspService, Server};

#[tokio::main]
async fn main() {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let mut parser = tree_sitter::Parser::new();
    let language = tree_sitter_lispbm::LANGUAGE;
    parser
        .set_language(&language.into())
        .expect("Error loading lispBM grammar");

    let (service, socket) = LspService::new(|client| Backend {
        client,
        parser: Mutex::new(parser),
    });

    // 4. Run the server on the tokio runtime
    Server::new(stdin, stdout, socket).serve(service).await;
}
