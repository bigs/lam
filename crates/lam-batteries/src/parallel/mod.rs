//! Parallel provider: search and extract.

mod client;
mod config;
mod namespace;
mod types;

pub use config::{ParallelConfig, ParallelFunction};
pub(crate) use namespace::parallel_namespace;
