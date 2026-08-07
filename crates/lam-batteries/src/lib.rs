//! Optional network capability packs for lam.
//!
//! Provider-native web search namespaces (`lam.exa`, `lam.parallel`) without a
//! least-common-denominator facade. Embedders install only the providers they
//! configure; missing credentials omit the corresponding namespace.

mod error;
mod exa;
mod http;
mod pack;
mod parallel;

pub use error::{BatteriesError, ProviderError};
pub use exa::{ExaConfig, ExaFunction};
pub use pack::{BatteriesPack, BatteriesPackBuildError, BatteriesPackBuilder};
pub use parallel::{ParallelConfig, ParallelFunction};
