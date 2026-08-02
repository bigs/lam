use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use lam::{Namespace, Never};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

#[derive(Clone)]
pub struct RoundTripGate {
    shared: Arc<RoundTripGateState>,
}

struct RoundTripGateState {
    signaled: AtomicBool,
    notify: Notify,
}

impl RoundTripGate {
    pub fn new() -> Self {
        Self {
            shared: Arc::new(RoundTripGateState {
                signaled: AtomicBool::new(false),
                notify: Notify::new(),
            }),
        }
    }

    pub fn wait_namespace(&self) -> Namespace {
        let wait = self.clone();
        Namespace::new("test.roundtrip", "Coordinates a bounded round-trip test.").function(
            "wait",
            "Wait until the child confirms that its parent message is durable.",
            move |_context, _input: Empty| {
                let gate = wait.clone();
                async move {
                    gate.wait().await;
                    Ok::<_, Never>(Acknowledged { acknowledged: true })
                }
            },
        )
    }

    pub fn signal_namespace(&self) -> Namespace {
        let signal = self.clone();
        Namespace::new("test.roundtrip", "Coordinates a bounded round-trip test.").function(
            "signal",
            "Confirm that the child has durably sent its parent message.",
            move |_context, _input: Empty| {
                let gate = signal.clone();
                async move {
                    gate.signal();
                    Ok::<_, Never>(Acknowledged { acknowledged: true })
                }
            },
        )
    }

    async fn wait(&self) {
        loop {
            let notified = self.shared.notify.notified();
            if self.shared.signaled.load(Ordering::Acquire) {
                return;
            }
            notified.await;
        }
    }

    fn signal(&self) {
        self.shared.signaled.store(true, Ordering::Release);
        self.shared.notify.notify_waiters();
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
struct Empty {}

#[derive(Debug, JsonSchema, Serialize)]
struct Acknowledged {
    acknowledged: bool,
}
