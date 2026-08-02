use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

/// The non-`Send` execution task for one actor runtime.
///
/// Poll this task on the same local executor where its actor was built. The
/// task owns the actor's Deno isolate and completes after the actor shuts down.
#[must_use = "an actor does not run unless its ActorTask is polled"]
pub struct ActorTask {
    future: Pin<Box<dyn Future<Output = ()> + 'static>>,
}

impl ActorTask {
    pub(crate) fn new(future: impl Future<Output = ()> + 'static) -> Self {
        Self {
            future: Box::pin(future),
        }
    }
}

impl Future for ActorTask {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        self.future.as_mut().poll(context)
    }
}
