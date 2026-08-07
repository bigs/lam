//! Exa provider: search, contents, context, answer, findSimilar.

mod client;
mod config;
mod namespace;
mod types;

pub use config::{ExaConfig, ExaFunction};
pub(crate) use namespace::exa_namespace;
