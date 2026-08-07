use std::sync::Arc;

use lam::Namespace;

use crate::exa::client::ExaClient;
use crate::exa::config::{ExaConfig, ExaFunction};
use crate::exa::types::{
    AnswerRequest, ContentsRequest, ContextRequest, FindSimilarRequest, SearchRequest,
};

/// Builds the `lam.exa` namespace for the configured function set.
pub(crate) fn exa_namespace(
    config: ExaConfig,
) -> Result<Option<Namespace>, crate::error::ProviderError> {
    if config.functions.is_empty() {
        return Ok(None);
    }
    let client = Arc::new(ExaClient::new(&config)?);
    let mut namespace = Namespace::new(
        "lam.exa",
        "Exa web search, page contents, code context, answers, and similar-link discovery. Provider-native request shapes; HTTP runs in Rust.",
    );

    if config.functions.contains(&ExaFunction::Search) {
        let client = Arc::clone(&client);
        namespace = namespace.function(
            "search",
            "Search the web with an Exa query. Modes: instant, fast, auto (default), deep-lite, deep, deep-reasoning. Optionally pull text, highlights, or summaries via contents. Prefer highlights for token-efficient excerpts; use contents() later for full pages.",
            move |_ctx, request: SearchRequest| {
                let client = Arc::clone(&client);
                async move { client.search(request).await }
            },
        );
    }
    if config.functions.contains(&ExaFunction::Contents) {
        let client = Arc::clone(&client);
        namespace = namespace.function(
            "contents",
            "Fetch page contents for known URLs or document ids from a prior search. Provide either urls or ids. Defaults to text:true when no content options are set. Use maxAgeHours:0 for live crawl.",
            move |_ctx, request: ContentsRequest| {
                let client = Arc::clone(&client);
                async move { client.contents(request).await }
            },
        );
    }
    if config.functions.contains(&ExaFunction::Context) {
        let client = Arc::clone(&client);
        namespace = namespace.function(
            "context",
            "Exa Code: retrieve token-efficient code and docs snippets from GitHub, documentation, and Stack Overflow. Pass tokensNum as \"dynamic\" or an integer budget (5000 is a good default).",
            move |_ctx, request: ContextRequest| {
                let client = Arc::clone(&client);
                async move { client.context(request).await }
            },
        );
    }
    if config.functions.contains(&ExaFunction::Answer) {
        let client = Arc::clone(&client);
        namespace = namespace.function(
            "answer",
            "Ask a question and get an Exa-synthesized answer with citations. Best for closed factual questions; use search+contents for multi-hop coding research. Optional outputSchema returns structured JSON.",
            move |_ctx, request: AnswerRequest| {
                let client = Arc::clone(&client);
                async move { client.answer(request).await }
            },
        );
    }
    if config.functions.contains(&ExaFunction::FindSimilar) {
        let client = Arc::clone(&client);
        namespace = namespace.function(
            "findSimilar",
            "Find pages semantically similar to a seed URL. Optional domain filters and contents extraction. Useful after search when a strong source page is already known.",
            move |_ctx, request: FindSimilarRequest| {
                let client = Arc::clone(&client);
                async move { client.find_similar(request).await }
            },
        );
    }

    Ok(Some(namespace))
}
