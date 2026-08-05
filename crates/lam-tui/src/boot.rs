//! Boot-phase timing for the diagnostic log.
//!
//! Every phase of the cold-start path records its wall-clock duration as a
//! JSONL event when --debug-log is enabled, so a slow boot can be attributed
//! to a specific step instead of guessed at. Events emitted before the
//! diagnostic writer is activated (once the session is selected) are dropped.

use std::time::Instant;

/// Runs one asynchronous boot phase and records its duration in the
/// diagnostic log. The phase result is returned unchanged, success or error.
pub(crate) async fn phase<T>(name: &str, future: impl std::future::Future<Output = T>) -> T {
    let started = Instant::now();
    let output = future.await;
    tracing::info!(
        target: "lam_tui::boot",
        event = "boot.phase",
        phase = name,
        elapsed_ms = started.elapsed().as_millis() as u64,
        "boot phase completed"
    );
    output
}

/// Runs one synchronous boot phase and records its duration in the
/// diagnostic log. The phase result is returned unchanged, success or error.
pub(crate) fn phase_sync<T>(name: &str, work: impl FnOnce() -> T) -> T {
    let started = Instant::now();
    let output = work();
    tracing::info!(
        target: "lam_tui::boot",
        event = "boot.phase",
        phase = name,
        elapsed_ms = started.elapsed().as_millis() as u64,
        "boot phase completed"
    );
    output
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::marker::PhantomData;
    use std::sync::{Arc, Mutex};

    use tracing_subscriber::fmt::MakeWriter;
    use tracing_subscriber::prelude::*;

    struct Buffer(Arc<Mutex<Vec<u8>>>);

    impl<'a> MakeWriter<'a> for Buffer {
        type Writer = BufferGuard<'a>;

        fn make_writer(&'a self) -> Self::Writer {
            BufferGuard(self.0.clone(), PhantomData)
        }
    }

    struct BufferGuard<'a>(Arc<Mutex<Vec<u8>>>, PhantomData<&'a ()>);

    impl Write for BufferGuard<'_> {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn phase_sync_emits_duration_fields() {
        let buffer = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry().with(
            tracing_subscriber::fmt::layer()
                .json()
                .with_ansi(false)
                .with_writer(Buffer(buffer.clone()))
                .with_filter(
                    tracing_subscriber::filter::Targets::new()
                        .with_target("lam_tui", tracing::Level::TRACE),
                ),
        );
        tracing::subscriber::with_default(subscriber, || {
            let output = super::phase_sync("test_phase", || 7u32);
            assert_eq!(output, 7);
        });
        let line = String::from_utf8(buffer.lock().unwrap().clone()).unwrap();
        assert!(line.contains("\"phase\":\"test_phase\""), "line: {line}");
        assert!(line.contains("\"elapsed_ms\":0"), "line: {line}");
    }
}
