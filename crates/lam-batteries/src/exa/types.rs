//! Provider-native Exa request/response types (camelCase wire format).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Content extraction options shared by search, contents, and findSimilar.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContentsOptions {
    /// Full page text (`true`) or advanced text options.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<Value>,
    /// Highlight snippets (`true`) or advanced highlight options.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub highlights: Option<Value>,
    /// LLM summary options (`{ query?, schema? }`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<Value>,
    /// Extra extraction (links, images, code blocks).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extras: Option<Value>,
    /// Maximum age of cached content in hours (`0` = live, `-1` = cache only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_age_hours: Option<i32>,
    /// Livecrawl timeout in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub livecrawl_timeout: Option<u32>,
    /// Number of subpages to crawl.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subpages: Option<u32>,
    /// Term(s) to prefer when selecting subpages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subpage_target: Option<Value>,
}

/// `POST /search` request body.
#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchRequest {
    /// Natural-language search query.
    pub query: String,
    /// Search mode: `instant`, `fast`, `auto`, `deep-lite`, `deep`, `deep-reasoning`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub r#type: Option<String>,
    /// Category hint: `company`, `people`, `publication`, `news`, …
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    /// Number of results (1–100; host-capped).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub num_results: Option<usize>,
    /// Domains or path prefixes to include.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include_domains: Option<Vec<String>>,
    /// Domains or path prefixes to exclude.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclude_domains: Option<Vec<String>>,
    /// Only results published after this ISO-8601 timestamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_published_date: Option<String>,
    /// Only results published before this ISO-8601 timestamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_published_date: Option<String>,
    /// Two-letter ISO country code for geo bias.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_location: Option<String>,
    /// Content moderation filter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub moderation: Option<bool>,
    /// Extra query variations for deep search modes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub additional_queries: Option<Vec<String>>,
    /// Guidance for synthesis or deep agent behavior.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    /// JSON schema for synthesized structured output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<Value>,
    /// Inline content extraction options for each hit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contents: Option<ContentsOptions>,
}

/// One search / contents / findSimilar result row.
#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResultItem {
    /// Result title when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Canonical URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Temporary document id useful for `/contents`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Estimated publish date.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published_date: Option<String>,
    /// Author when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    /// Full text when requested.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Highlight snippets when requested.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub highlights: Option<Vec<String>>,
    /// Highlight scores when returned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub highlight_scores: Option<Vec<f64>>,
    /// Summary when requested.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// Favicon URL when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub favicon: Option<String>,
    /// Image URL when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// Subpages when requested.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subpages: Option<Value>,
    /// Structured entity payloads for category search.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entities: Option<Value>,
    /// Additional provider fields.
    #[serde(default, flatten)]
    pub extra: std::collections::BTreeMap<String, Value>,
}

/// Search / findSimilar response.
#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchResponse {
    /// Provider request id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    /// Ranked results.
    #[serde(default)]
    pub results: Vec<ResultItem>,
    /// Synthesized output when `outputSchema` was provided.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<Value>,
    /// Estimated cost breakdown when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_dollars: Option<Value>,
    /// Additional provider fields.
    #[serde(default, flatten)]
    pub extra: std::collections::BTreeMap<String, Value>,
}

/// `POST /contents` request body.
#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContentsRequest {
    /// URLs to fetch (mutually exclusive with `ids`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub urls: Option<Vec<String>>,
    /// Document ids from a prior search (mutually exclusive with `urls`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ids: Option<Vec<String>>,
    /// Full page text (`true`) or advanced text options.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<Value>,
    /// Highlight snippets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub highlights: Option<Value>,
    /// LLM summary options.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<Value>,
    /// Extra extraction options.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extras: Option<Value>,
    /// Maximum age of cached content in hours.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_age_hours: Option<i32>,
    /// Livecrawl timeout in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub livecrawl_timeout: Option<u32>,
    /// Number of subpages to crawl.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subpages: Option<u32>,
    /// Term(s) to prefer when selecting subpages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subpage_target: Option<Value>,
}

/// Contents response.
#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContentsResponse {
    /// Provider request id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    /// Per-URL content results.
    #[serde(default)]
    pub results: Vec<ResultItem>,
    /// Per-URL fetch status.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub statuses: Option<Value>,
    /// Estimated cost when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_dollars: Option<Value>,
    /// Additional provider fields.
    #[serde(default, flatten)]
    pub extra: std::collections::BTreeMap<String, Value>,
}

/// `POST /context` (Exa Code) request body.
#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextRequest {
    /// Code/docs search query.
    pub query: String,
    /// Token budget: integer 50–100000, or the string `"dynamic"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_num: Option<Value>,
}

/// Context (Exa Code) response.
#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextResponse {
    /// Provider request id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    /// Echo of the query.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    /// Concatenated code/docs context.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response: Option<String>,
    /// Number of underlying hits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub results_count: Option<u64>,
    /// Output token count when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    /// Search latency when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search_time: Option<f64>,
    /// Estimated cost when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_dollars: Option<Value>,
    /// Additional provider fields.
    #[serde(default, flatten)]
    pub extra: std::collections::BTreeMap<String, Value>,
}

/// `POST /answer` request body.
#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AnswerRequest {
    /// Natural-language question.
    pub query: String,
    /// When true, include source page text on citations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<bool>,
    /// JSON Schema for a structured answer object instead of a string.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<Value>,
}

/// Answer response.
#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AnswerResponse {
    /// Provider request id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    /// Generated answer (string or structured object).
    pub answer: Value,
    /// Sources used for the answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub citations: Option<Vec<ResultItem>>,
    /// Estimated cost when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_dollars: Option<Value>,
    /// Additional provider fields.
    #[serde(default, flatten)]
    pub extra: std::collections::BTreeMap<String, Value>,
}

/// `POST /findSimilar` request body.
#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FindSimilarRequest {
    /// Seed URL to find similar pages for.
    pub url: String,
    /// Number of results (host-capped).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub num_results: Option<usize>,
    /// Domains to include.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include_domains: Option<Vec<String>>,
    /// Domains to exclude.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclude_domains: Option<Vec<String>>,
    /// Exclude results from the seed URL's domain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclude_source_domain: Option<bool>,
    /// Category hint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    /// Only results published after this ISO-8601 timestamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_published_date: Option<String>,
    /// Only results published before this ISO-8601 timestamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_published_date: Option<String>,
    /// Inline content options.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contents: Option<ContentsOptions>,
}
