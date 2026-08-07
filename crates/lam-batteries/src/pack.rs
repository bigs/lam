use std::fmt;

use lam::Namespace;
use thiserror::Error;

use crate::error::ProviderError;
use crate::exa::{ExaConfig, exa_namespace};
use crate::parallel::{ParallelConfig, parallel_namespace};

/// Failure while building a batteries pack.
#[derive(Debug, Error)]
pub enum BatteriesPackBuildError {
    /// Provider configuration or client construction failed.
    #[error(transparent)]
    Provider(#[from] ProviderError),
}

/// A configured collection of optional batteries namespaces.
pub struct BatteriesPack {
    namespaces: Vec<Namespace>,
}

impl fmt::Debug for BatteriesPack {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BatteriesPack")
            .field(
                "namespaces",
                &self
                    .namespaces
                    .iter()
                    .map(Namespace::path)
                    .collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl BatteriesPack {
    /// Starts an empty pack builder.
    #[must_use]
    pub fn builder() -> BatteriesPackBuilder {
        BatteriesPackBuilder::default()
    }

    /// Clones the cheaply shared namespaces for registration with a lam builder.
    pub fn namespaces(&self) -> impl Iterator<Item = Namespace> + '_ {
        self.namespaces.iter().cloned()
    }

    /// Returns true when at least one namespace was installed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.namespaces.is_empty()
    }
}

impl<'a> IntoIterator for &'a BatteriesPack {
    type Item = Namespace;
    type IntoIter = std::iter::Cloned<std::slice::Iter<'a, Namespace>>;

    fn into_iter(self) -> Self::IntoIter {
        self.namespaces.iter().cloned()
    }
}

/// Builder for optional batteries namespaces.
#[derive(Clone, Debug, Default)]
pub struct BatteriesPackBuilder {
    exa: Option<ExaConfig>,
    parallel: Option<ParallelConfig>,
}

impl BatteriesPackBuilder {
    /// Installs `lam.exa` when the config enables one or more functions.
    #[must_use]
    pub fn exa(mut self, config: ExaConfig) -> Self {
        self.exa = Some(config);
        self
    }

    /// Installs `lam.parallel` when the config enables one or more functions.
    #[must_use]
    pub fn parallel(mut self, config: ParallelConfig) -> Self {
        self.parallel = Some(config);
        self
    }

    /// Materializes configured namespaces.
    pub fn build(self) -> Result<BatteriesPack, BatteriesPackBuildError> {
        let mut namespaces = Vec::new();
        if let Some(config) = self.exa
            && let Some(namespace) = exa_namespace(config)?
        {
            namespaces.push(namespace);
        }
        if let Some(config) = self.parallel
            && let Some(namespace) = parallel_namespace(config)?
        {
            namespaces.push(namespace);
        }
        Ok(BatteriesPack { namespaces })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exa::ExaFunction;
    use crate::parallel::ParallelFunction;

    #[test]
    fn empty_builder_produces_empty_pack() {
        let pack = BatteriesPack::builder().build().unwrap();
        assert!(pack.is_empty());
    }

    #[test]
    fn installs_both_namespaces_with_default_functions() {
        let pack = BatteriesPack::builder()
            .exa(ExaConfig::from_api_key("exa-test"))
            .parallel(ParallelConfig::from_api_key("parallel-test"))
            .build()
            .unwrap();
        let paths: Vec<_> = pack.namespaces().map(|ns| ns.path().to_owned()).collect();
        assert_eq!(paths, ["lam.exa", "lam.parallel"]);
    }

    #[test]
    fn empty_function_set_omits_namespace() {
        let pack = BatteriesPack::builder()
            .exa(ExaConfig::from_api_key("exa-test").functions([]))
            .build()
            .unwrap();
        assert!(pack.is_empty());
    }

    #[test]
    fn restricted_function_set_still_installs_namespace() {
        let pack = BatteriesPack::builder()
            .exa(ExaConfig::from_api_key("exa-test").functions([ExaFunction::Search]))
            .parallel(ParallelConfig::from_api_key("p").functions([ParallelFunction::Extract]))
            .build()
            .unwrap();
        let paths: Vec<_> = pack.namespaces().map(|ns| ns.path().to_owned()).collect();
        assert_eq!(paths, ["lam.exa", "lam.parallel"]);
    }
}
