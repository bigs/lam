use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};

use deno_core::futures::FutureExt;
use deno_core::futures::channel::{mpsc, oneshot};
use deno_core::parking_lot::Mutex;
use deno_core::{
    InspectorMsg, InspectorMsgKind, InspectorSessionKind, JsRuntime, JsRuntimeInspector,
    LocalInspectorSession,
};
use serde::Serialize;
use serde_json::Value;

static NEXT_MESSAGE_ID: AtomicI32 = AtomicI32::new(1);

fn next_message_id() -> i32 {
    NEXT_MESSAGE_ID.fetch_add(1, Ordering::Relaxed)
}

#[derive(Debug)]
enum ResponseState {
    Ready(Value),
    Waiting(oneshot::Sender<Value>),
}

#[derive(Debug)]
struct InspectorStateInner {
    responses: HashMap<i32, ResponseState>,
    notifications: mpsc::UnboundedSender<Value>,
}

#[derive(Clone, Debug)]
struct InspectorState(Arc<Mutex<InspectorStateInner>>);

impl InspectorState {
    fn new(notifications: mpsc::UnboundedSender<Value>) -> Self {
        Self(Arc::new(Mutex::new(InspectorStateInner {
            responses: HashMap::new(),
            notifications,
        })))
    }

    fn callback(&self, message: InspectorMsg) {
        let parsed = match serde_json::from_str::<Value>(&message.content) {
            Ok(value) => value,
            Err(_) => return,
        };

        let InspectorMsgKind::Message(message_id) = message.kind else {
            let _ = self.0.lock().notifications.unbounded_send(parsed);
            return;
        };

        let mut state = self.0.lock();
        match state.responses.remove(&message_id) {
            Some(ResponseState::Waiting(sender)) => {
                let _ = sender.send(parsed);
            }
            Some(ResponseState::Ready(_)) | None => {
                state
                    .responses
                    .insert(message_id, ResponseState::Ready(parsed));
            }
        }
    }

    async fn wait_for_response(&self, message_id: i32) -> Result<Value, InspectorError> {
        let receiver = {
            let mut state = self.0.lock();
            if let Some(ResponseState::Ready(value)) = state.responses.remove(&message_id) {
                return extract_result(value);
            }

            let (sender, receiver) = oneshot::channel();
            state
                .responses
                .insert(message_id, ResponseState::Waiting(sender));
            receiver
        };

        let value = receiver.await.map_err(|_| InspectorError::Disconnected)?;
        extract_result(value)
    }
}

fn extract_result(mut value: Value) -> Result<Value, InspectorError> {
    if !value["error"].is_null() {
        return Err(InspectorError::Protocol {
            message: value["error"].to_string(),
        });
    }

    let result = value["result"].take();
    if result.is_null() {
        Err(InspectorError::Protocol {
            message: format!("CDP response did not contain a result: {value}"),
        })
    } else {
        Ok(result)
    }
}

/// A minimal in-process Chrome DevTools Protocol client.
///
/// This type must be dropped before its associated `JsRuntime`.
pub(crate) struct InspectorClient {
    session: LocalInspectorSession,
    state: InspectorState,
    context_id: u64,
}

impl InspectorClient {
    pub(crate) fn attach(runtime: &mut JsRuntime) -> Result<Self, InspectorError> {
        let (notification_sender, mut notification_receiver) = mpsc::unbounded();
        let state = InspectorState::new(notification_sender);
        let callback_state = state.clone();
        let callback = Box::new(move |message| callback_state.callback(message));
        let kind = InspectorSessionKind::NonBlocking {
            wait_for_disconnect: false,
        };
        let mut session =
            JsRuntimeInspector::create_local_session(runtime.inspector(), callback, kind);

        session.post_message::<()>(next_message_id(), "Runtime.enable", None);

        // Local inspector dispatch invokes its callback synchronously. Requiring
        // the context notification here ensures a new runtime is parked before
        // its builder can ever yield to another isolate on the same thread.
        let context_id = loop {
            let notification = notification_receiver
                .try_recv()
                .map_err(|error| InspectorError::Protocol {
                    message: format!(
                        "Runtime.enable did not synchronously announce the default execution context: {error}"
                    ),
                })?;
            if notification["method"] != "Runtime.executionContextCreated" {
                continue;
            }

            let context = &notification["params"]["context"];
            if context["auxData"]["isDefault"].as_bool() != Some(true) {
                continue;
            }
            break context["id"]
                .as_u64()
                .ok_or_else(|| InspectorError::Protocol {
                    message: format!(
                        "execution context notification had no numeric id: {notification}"
                    ),
                })?;
        };

        Ok(Self {
            session,
            state,
            context_id,
        })
    }

    pub(crate) const fn context_id(&self) -> u64 {
        self.context_id
    }

    pub(crate) async fn post<T: Serialize>(
        &mut self,
        runtime: &mut JsRuntime,
        method: &str,
        params: T,
    ) -> Result<Value, InspectorError> {
        let message_id = next_message_id();
        self.session.post_message(message_id, method, Some(params));

        // V8's REPL mode wraps evaluations in an async IIFE. Under the explicit
        // microtask policy, draining here prevents that weakly-held promise from
        // being collected before its first resolution microtask.
        runtime.v8_isolate().perform_microtask_checkpoint();

        let response = self.state.wait_for_response(message_id).boxed_local();
        runtime
            .with_event_loop_future(response, Default::default())
            .await
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum InspectorError {
    #[error("the local inspector session disconnected")]
    Disconnected,
    #[error("Chrome DevTools Protocol error: {message}")]
    Protocol { message: String },
}
