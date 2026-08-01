use lam::{
    EncodedPayload, MessageSource, SYSTEM_NOTICE_CODEC_ID, SYSTEM_NOTICE_CODEC_VERSION,
    SystemNotice,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::error::CodecError;

pub(crate) const LAM_MESSAGES_CODEC_ID: &str = "lam/messages";
pub(crate) const LAM_EVAL_CODEC_ID: &str = "lam/eval";
pub(crate) const LAM_CODEC_VERSION: u32 = 1;

#[derive(Clone, Copy)]
pub(crate) enum NativeRole {
    User,
    System,
}

pub(crate) struct NativeMessage {
    pub(crate) role: NativeRole,
    pub(crate) text: String,
    pub(crate) closes_interrupted_eval: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeliveredMessage {
    source: MessageSource,
    payload: EncodedPayload,
}

pub(crate) fn messages(value: &Value) -> Result<Vec<NativeMessage>, CodecError> {
    let delivered: Vec<DeliveredMessage> =
        serde_json::from_value(value.clone()).map_err(|error| CodecError::InvalidPayload {
            message: format!("lam/messages payload is invalid: {error}"),
        })?;
    delivered.into_iter().map(render_message).collect()
}

pub(crate) fn eval_output(value: &Value) -> Result<String, CodecError> {
    serde_json::to_string(value).map_err(|error| CodecError::InvalidPayload {
        message: format!("lam/eval payload cannot be encoded: {error}"),
    })
}

pub(crate) fn is_codec(payload: &EncodedPayload, id: &str, version: u32) -> bool {
    payload.codec.id.as_str() == id && payload.codec.version == version
}

pub(crate) fn unsupported(payload: &EncodedPayload) -> CodecError {
    CodecError::UnsupportedContext {
        codec: format!("{}@{}", payload.codec.id, payload.codec.version),
    }
}

fn render_message(message: DeliveredMessage) -> Result<NativeMessage, CodecError> {
    let interrupted = is_interrupted_eval_notice(&message.payload);
    let (role, text) = match &message.source {
        MessageSource::User { .. } => (NativeRole::User, render_user_payload(&message.payload)?),
        MessageSource::Host { component } => {
            let tag = if is_system_notice(&message.payload) {
                "lam_system_notice"
            } else {
                "lam_host_message"
            };
            let value = json!({
                "component": component,
                "payload": message.payload.value,
            });
            (NativeRole::System, tagged(tag, &value)?)
        }
        MessageSource::Actor { actor_id } => {
            let value = json!({
                "actorId": actor_id,
                "payload": message.payload.value,
            });
            (NativeRole::User, tagged("lam_actor_message", &value)?)
        }
    };
    Ok(NativeMessage {
        role,
        text,
        closes_interrupted_eval: interrupted,
    })
}

fn render_user_payload(payload: &EncodedPayload) -> Result<String, CodecError> {
    if is_codec(payload, "lam/json", LAM_CODEC_VERSION) {
        return match &payload.value {
            Value::String(text) => Ok(text.clone()),
            value => serde_json::to_string(value).map_err(|error| CodecError::InvalidPayload {
                message: format!("Lam JSON input cannot be encoded: {error}"),
            }),
        };
    }
    let value = json!({
        "codec": payload.codec,
        "value": payload.value,
    });
    tagged("lam_payload", &value)
}

fn is_system_notice(payload: &EncodedPayload) -> bool {
    is_codec(payload, SYSTEM_NOTICE_CODEC_ID, SYSTEM_NOTICE_CODEC_VERSION)
}

fn is_interrupted_eval_notice(payload: &EncodedPayload) -> bool {
    if !is_system_notice(payload) {
        return false;
    }
    matches!(
        payload.decode::<SystemNotice>(),
        Ok(SystemNotice::RuntimeResumed {
            interrupted_eval_outcome: Some(_),
            ..
        })
    )
}

fn tagged(tag: &str, value: &Value) -> Result<String, CodecError> {
    let json = serde_json::to_string(value).map_err(|error| CodecError::InvalidPayload {
        message: format!("Lam message cannot be encoded: {error}"),
    })?;
    Ok(format!("<{tag}>{json}</{tag}>"))
}
