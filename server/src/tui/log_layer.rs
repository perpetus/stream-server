use crossbeam_channel::Sender;
use tracing::{Event, Subscriber};
use tracing_subscriber::{layer::Context, Layer};

pub struct TuiLogLayer {
    sender: Sender<String>,
}

impl TuiLogLayer {
    pub fn new(sender: Sender<String>) -> Self {
        Self { sender }
    }
}

impl<S> Layer<S> for TuiLogLayer
where
    S: Subscriber,
{
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let mut buffer = String::new();
        // A simple visitor to format the message.
        // In a real TUI we might want structured data, but string is easiest for list widget.
        let mut visitor = MessageVisitor(&mut buffer);
        event.record(&mut visitor);

        // Try send, drop if full to avoid blocking
        let _ = self.sender.try_send(buffer);
    }
}

struct MessageVisitor<'a>(&'a mut String);

impl<'a> tracing::field::Visit for MessageVisitor<'a> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            use std::fmt::Write;
            let _ = write!(self.0, "{:?}", value);
        }
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" {
            self.0.push_str(value);
        }
    }
}
