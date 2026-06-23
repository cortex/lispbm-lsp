use tokio::sync::mpsc;
use tower_lsp_server::ls_types;
use tracing::Subscriber;
use tracing_subscriber::Layer;

#[derive(Debug)]
pub struct Logger {
    pub client: tower_lsp_server::Client,
    pub rx: mpsc::UnboundedReceiver<(ls_types::MessageType, String)>,
}

impl Logger {
    pub fn new(
        client: tower_lsp_server::Client,
        rx: mpsc::UnboundedReceiver<(ls_types::MessageType, String)>,
    ) -> Self {
        Self { client, rx }
    }

    pub async fn run(mut self) {
        while let Some((level, message)) = self.rx.recv().await {
            self.client.log_message(level, message).await;
        }
    }
}

#[derive(Debug)]
pub struct LspTracer {
    pub tx: mpsc::UnboundedSender<(ls_types::MessageType, String)>,
}

impl LspTracer {
    pub fn new(tx: mpsc::UnboundedSender<(ls_types::MessageType, String)>) -> Self {
        Self { tx }
    }
}

impl<S> Layer<S> for LspTracer
where
    S: Subscriber,
{
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let level = event.metadata().level();
        let level = match *level {
            tracing::Level::ERROR => ls_types::MessageType::ERROR,
            tracing::Level::WARN => ls_types::MessageType::WARNING,
            tracing::Level::INFO => ls_types::MessageType::INFO,
            _ => return, // Ignore DEBUG and TRACE levels
        };

        let mut message = String::new();
        let mut visitor = StringVisitor(&mut message);
        event.record(&mut visitor);

        let _ = self.tx.send((level, message));
    }
}

// Helper to extract the message text from tracing macros
struct StringVisitor<'a>(&'a mut String);
impl<'a> tracing::field::Visit for StringVisitor<'a> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            use std::fmt::Write;
            let _ = write!(self.0, "{:?}", value);
        }
    }
}
