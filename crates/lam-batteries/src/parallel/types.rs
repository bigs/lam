//! Provider-native Parallel request/response types (snake_case wire format).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Domain / date filtering for search.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema, Serialize)]
pub struct SourcePolicy {
    /// Only include these domains or extensions (e.g. `.edu`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include_domains: Option<Vec<String>>,
    /// Exclude these domains or extensions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclude_domains: Option<Vec<String>>,
    /// Only content published on or after this date (`YYYY-MM-DD`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_date: Option<String>,
}

/// Cache vs live-fetch policy.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema, Serialize)]
pub struct FetchPolicy {
    /// Maximum cache age in seconds before a live fetch (minimum 600).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_age_seconds: Option<u64>,
    /// Live-fetch timeout in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_seconds: Option<f64>,
    /// When true, do not fall back to stale cache after a live-fetch failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disable_cache_fallback: Option<bool>,
}

/// Excerpt size controls.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema, Serialize)]
pub struct ExcerptSettings {
    /// Upper bound on characters of excerpts per URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_chars_per_result: Option<u64>,
}

/// Advanced knobs for search.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema, Serialize)]
pub struct AdvancedSearchSettings {
    /// Domain and date filters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_policy: Option<SourcePolicy>,
    /// Cache vs live-fetch policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fetch_policy: Option<FetchPolicy>,
    /// Excerpt size controls.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub excerpt_settings: Option<ExcerptSettings>,
    /// ISO 3166-1 alpha-2 country code for geo bias.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
    /// Upper bound on result count (host-capped).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_results: Option<usize>,
}

/// Advanced knobs for extract.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema, Serialize)]
pub struct AdvancedExtractSettings {
    /// Cache vs live-fetch policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fetch_policy: Option<FetchPolicy>,
    /// Excerpt size controls.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub excerpt_settings: Option<ExcerptSettings>,
    /// Full content: `true`, `false`, or settings object.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub full_content: Option<Value>,
}

/// `POST /v1/search` request body.
#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
pub struct SearchRequest {
    /// Natural-language research goal (self-contained).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub objective: Option<String>,
    /// Keyword queries, 3–6 words each; provide 2–3 diverse queries for best results.
    pub search_queries: Vec<String>,
    /// Mode: `turbo`, `basic`, or `advanced` (default advanced).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// Upper bound on total excerpt characters across all results.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_chars_total: Option<u64>,
    /// Session id to thread search→extract for the same larger task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Downstream model id for provider optimizations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_model: Option<String>,
    /// Source policy, fetch policy, location, max_results.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub advanced_settings: Option<AdvancedSearchSettings>,
}

/// One search result.
#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
pub struct WebSearchResult {
    /// Result URL.
    pub url: String,
    /// Page title when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Publish date (`YYYY-MM-DD`) when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publish_date: Option<String>,
    /// LLM-oriented markdown excerpts.
    #[serde(default)]
    pub excerpts: Vec<String>,
}

/// Search response.
#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
pub struct SearchResponse {
    /// Provider search id.
    pub search_id: String,
    /// Ranked results.
    #[serde(default)]
    pub results: Vec<WebSearchResult>,
    /// Session id to pass into subsequent extract/search calls.
    pub session_id: String,
    /// Provider warnings when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warnings: Option<Value>,
    /// Usage metrics when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Value>,
}

/// `POST /v1/extract` request body.
#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
pub struct ExtractRequest {
    /// URLs to extract (up to 20; host-capped).
    pub urls: Vec<String>,
    /// Natural-language focus for excerpts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub objective: Option<String>,
    /// Optional keyword queries to focus excerpts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search_queries: Option<Vec<String>>,
    /// Upper bound on total excerpt characters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_chars_total: Option<u64>,
    /// Session id from a prior search/extract call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Downstream model id for provider optimizations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_model: Option<String>,
    /// Fetch policy, excerpt settings, full_content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub advanced_settings: Option<AdvancedExtractSettings>,
}

/// One successful extract result.
#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
pub struct ExtractResult {
    /// Source URL.
    pub url: String,
    /// Page title when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Publish date when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publish_date: Option<String>,
    /// Focused markdown excerpts.
    #[serde(default)]
    pub excerpts: Vec<String>,
    /// Full page markdown when requested.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub full_content: Option<String>,
}

/// Per-URL extract failure.
#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
pub struct ExtractError {
    /// Failed URL.
    pub url: String,
    /// Error type from Parallel.
    pub error_type: String,
    /// HTTP status when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_status_code: Option<u16>,
    /// Optional body excerpt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
}

/// Extract response.
#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
pub struct ExtractResponse {
    /// Provider extract id.
    pub extract_id: String,
    /// Successful results.
    #[serde(default)]
    pub results: Vec<ExtractResult>,
    /// Per-URL failures (not a top-level error).
    #[serde(default)]
    pub errors: Vec<ExtractError>,
    /// Session id for subsequent calls.
    pub session_id: String,
    /// Provider warnings when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warnings: Option<Value>,
    /// Usage metrics when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Value>,
}
