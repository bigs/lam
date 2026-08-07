use std::collections::BTreeSet;
use std::time::Duration;

/// Which `lam.parallel` functions to install.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum ParallelFunction {
    /// `POST /v1/search`
    Search,
    /// `POST /v1/extract`
    Extract,
}

impl ParallelFunction {
    /// Default set for interactive coding agents.
    pub const DEFAULTS: &[Self] = &[Self::Search, Self::Extract];

    /// Config / inventory name for this function.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Search => "search",
            Self::Extract => "extract",
        }
    }

    /// Parses a config allowlist entry.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "search" => Some(Self::Search),
            "extract" => Some(Self::Extract),
            _ => None,
        }
    }
}

/// Credentials and policy for the Parallel namespace.
#[derive(Clone, Debug)]
pub struct ParallelConfig {
    /// API key for `https://api.parallel.ai`.
    pub api_key: String,
    /// Optional base URL override (tests). Defaults to the public Parallel API.
    pub base_url: String,
    /// Installed functions; empty means install none.
    pub functions: BTreeSet<ParallelFunction>,
    /// HTTP timeout for one provider call.
    pub timeout: Duration,
    /// Hard ceiling on `advanced_settings.max_results` when set by the model.
    pub max_results: usize,
    /// Hard ceiling on URL counts for extract (Parallel allows up to 20).
    pub max_urls: usize,
}

impl ParallelConfig {
    /// Builds a config with the default function set and public base URL.
    #[must_use]
    pub fn from_api_key(api_key: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            base_url: "https://api.parallel.ai".to_owned(),
            functions: ParallelFunction::DEFAULTS.iter().copied().collect(),
            timeout: Duration::from_secs(90),
            max_results: 25,
            max_urls: 20,
        }
    }

    /// Restricts installed functions to the given set.
    #[must_use]
    pub fn functions(mut self, functions: impl IntoIterator<Item = ParallelFunction>) -> Self {
        self.functions = functions.into_iter().collect();
        self
    }

    /// Replaces the HTTP timeout.
    #[must_use]
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}
