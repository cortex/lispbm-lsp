mod definitions;
mod entry;
mod lsp;
mod state;

use tokio::sync::Mutex;
use tower_lsp_server::{LspService, Server};

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let mut parser = tree_sitter::Parser::new();
    let language = tree_sitter_lispbm::LANGUAGE;
    parser
        .set_language(&language.into())
        .expect("Error loading lispBM grammar");

    let (service, socket) = LspService::new(|client| lsp::Backend {
        client,
        parser: Mutex::new(parser),
        state: Mutex::new(state::State::default()),
    });

    Server::new(stdin, stdout, socket).serve(service).await;
}
