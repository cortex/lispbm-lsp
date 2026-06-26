mod builtin;
mod definitions;
mod entry;
mod logger;
mod lsp;
mod state;

use tower_lsp_server::{LspService, Server};
use tracing_subscriber::layer::SubscriberExt;

#[tokio::main]
async fn main() {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let mut parser = tree_sitter::Parser::new();
    let language = tree_sitter_lispbm::LANGUAGE;
    parser
        .set_language(&language.into())
        .expect("Error loading lispBM grammar");

    let (service, socket) = LspService::new(|client| {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let worker = logger::Logger::new(client.clone(), rx);
        let lsp_layer = logger::LspTracer::new(tx);
        tokio::spawn(worker.run());

        let subscriber = tracing_subscriber::Registry::default().with(lsp_layer);
        tracing::subscriber::set_global_default(subscriber).unwrap();
        lsp::Backend::new(client)
    });

    Server::new(stdin, stdout, socket).serve(service).await;
}
