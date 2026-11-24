//! Process-boundary diagnostics. Protocol stdout is never a log sink.
use tracing_subscriber::{filter::LevelFilter, layer::SubscriberExt, util::SubscriberInitExt};

/// Install JSON stderr diagnostics for Pigeon-owned events only.
/// Library callers and protocol-only helper processes do not initialize this.
pub fn init() {
    let _ = subscriber(std::io::stderr).try_init();
}

fn subscriber<W>(writer: W) -> impl tracing::Subscriber + Send + Sync
where
    W: for<'a> tracing_subscriber::fmt::MakeWriter<'a> + Send + Sync + 'static,
{
    use tracing_subscriber::Layer;
    let layer = tracing_subscriber::fmt::layer()
        .json()
        .with_current_span(false)
        .with_span_list(false)
        .with_target(false)
        .with_writer(writer)
        .with_filter(
            tracing_subscriber::filter::Targets::new()
                .with_target("radio999k", LevelFilter::INFO)
                .with_target("999k", LevelFilter::INFO),
        );
    tracing_subscriber::registry().with(layer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::Write,
        sync::{Arc, Mutex},
    };
    #[derive(Clone)]
    struct Capture(Arc<Mutex<Vec<u8>>>);
    impl Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
        type Writer = Self;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }
    #[test]
    fn json_sink_omits_foreign_events_and_span_payloads() {
        let capture = Capture(Arc::new(Mutex::new(Vec::new())));
        tracing::subscriber::with_default(subscriber(capture.clone()), || {
            let span = tracing::info_span!(target: "radio999k", "fixture", secret = "PUBLIC_SPAN_SENTINEL");
            let _entered = span.enter();
            tracing::info!(target: "radio999k", component="fixture", event="fact", outcome="verified");
            tracing::info!(target: "reqwest", authorization="PUBLIC_FOREIGN_SENTINEL");
        });
        let bytes = capture.0.lock().unwrap();
        let text = String::from_utf8(bytes.clone()).unwrap();
        assert!(!text.contains("PUBLIC_SPAN_SENTINEL"));
        assert!(!text.contains("PUBLIC_FOREIGN_SENTINEL"));
        let rows: Vec<serde_json::Value> = text
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(rows.len(), 1);
        assert!(rows[0]["timestamp"].as_str().unwrap().ends_with('Z'));
        assert_eq!(rows[0]["fields"]["event"], "fact");
        assert!(rows[0].get("spans").is_none());
    }
}
