use std::collections::BTreeSet;
use std::time::Duration;

/// Which `lam.exa` functions to install.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum ExaFunction {
    /// `POST /search`
    Search,
    /// `POST /contents`
    Contents,
    /// `POST /context` (Exa Code)
    Context,
    /// `POST /answer`
    Answer,
    /// `POST /findSimilar`
    FindSimilar,
}

impl ExaFunction {
    /// Default set for interactive coding agents (must + should).
    pub const DEFAULTS: &[Self] = &[
        Self::Search,
        Self::Contents,
        Self::Context,
        Self::Answer,
        Self::FindSimilar,
    ];

    /// Config / inventory name for this function.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Search => "search",
            Self::Contents => "contents",
            Self::Context => "context",
            Self::Answer => "answer",
            Self::FindSimilar => "findSimilar",
        }
    }

    /// Parses a config allowlist entry.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "search" => Some(Self::Search),
            "contents" => Some(Self::Contents),
            "context" => Some(Self::Context),
            "answer" => Some(Self::Answer),
            "findSimilar" | "find_similar" | "find-similar" => Some(Self::FindSimilar),
            _ => None,
        }
    }
}

/// Credentials and policy for the Exa namespace.
#[derive(Clone, Debug)]
pub struct ExaConfig {
    /// API key for `https://api.exa.ai`.
    pub api_key: String,
    /// Optional base URL override (tests). Defaults to the public Exa API.
    pub base_url: String,
    /// Installed functions; empty means install none.
    pub functions: BTreeSet<ExaFunction>,
    /// HTTP timeout for one provider call.
    pub timeout: Duration,
    /// Hard ceiling on `numResults` for search and findSimilar.
    pub max_results: usize,
    /// Hard ceiling on URL/id counts for contents.
    pub max_urls: usize,
}

impl ExaConfig {
    /// Builds a config with the default function set and public base URL.
    #[must_use]
    pub fn from_api_key(api_key: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            base_url: "https://api.exa.ai".to_owned(),
            functions: ExaFunction::DEFAULTS.iter().copied().collect(),
            timeout: Duration::from_secs(90),
            max_results: 25,
            max_urls: 25,
        }
    }

    /// Restricts installed functions to the given set.
    #[must_use]
    pub fn functions(mut self, functions: impl IntoIterator<Item = ExaFunction>) -> Self {
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
