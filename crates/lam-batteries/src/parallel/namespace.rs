use std::sync::Arc;

use lam::Namespace;

use crate::parallel::client::ParallelClient;
use crate::parallel::config::{ParallelConfig, ParallelFunction};
use crate::parallel::types::{ExtractRequest, SearchRequest};

/// Builds the `lam.parallel` namespace for the configured function set.
pub(crate) fn parallel_namespace(
    config: ParallelConfig,
) -> Result<Option<Namespace>, crate::error::ProviderError> {
    if config.functions.is_empty() {
        return Ok(None);
    }
    let client = Arc::new(ParallelClient::new(&config)?);
    let mut namespace = Namespace::new(
        "lam.parallel",
        "Parallel web search and extract. Use search with objective + 2–3 keyword search_queries, then extract on chosen URLs. Thread session_id across calls in the same task.",
    );

    if config.functions.contains(&ParallelFunction::Search) {
        let client = Arc::clone(&client);
        namespace = namespace.function(
            "search",
            "Search the web with a natural-language objective plus keyword search_queries (required; 2–3 diverse 3–6 word queries). Modes: turbo, basic, advanced. Returns ranked URLs with markdown excerpts and a session_id for extract.",
            move |_ctx, request: SearchRequest| {
                let client = Arc::clone(&client);
                async move { client.search(request).await }
            },
        );
    }
    if config.functions.contains(&ParallelFunction::Extract) {
        let client = Arc::clone(&client);
        namespace = namespace.function(
            "extract",
            "Extract LLM-ready markdown from known URLs (up to 20). Pass objective to focus excerpts; set advanced_settings.full_content to true for full pages. Prefer after search; pass session_id from a prior search when continuing the same task.",
            move |_ctx, request: ExtractRequest| {
                let client = Arc::clone(&client);
                async move { client.extract(request).await }
            },
        );
    }

    Ok(Some(namespace))
}
