//! Lossless model adapters for OpenAI's Responses API and the broadly
//! supported OpenAI-compatible Chat Completions protocol.
//!
//! Both adapters keep provider-native JSON authoritative. The Responses
//! adapter stores the completed response object untouched, while the streaming
//! Chat Completions adapter stores every native chunk because that protocol
//! does not return a second, completed response object after `[DONE]`.

mod common;
mod context;
mod error;
mod metadata;
mod transport;

pub mod chat_completions;
pub mod responses;

pub use error::{BuildError, CodecError, ProviderError};
pub use metadata::ModelPricing;
