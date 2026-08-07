use crate::error::ProviderError;
use crate::http::{HttpClient, api_key_header, cap_usize};
use crate::parallel::config::ParallelConfig;
use crate::parallel::types::{ExtractRequest, ExtractResponse, SearchRequest, SearchResponse};

#[derive(Clone, Debug)]
pub(crate) struct ParallelClient {
    http: HttpClient,
    base_url: String,
    headers: reqwest::header::HeaderMap,
    max_results: usize,
    max_urls: usize,
}

impl ParallelClient {
    pub(crate) fn new(config: &ParallelConfig) -> Result<Self, ProviderError> {
        if config.api_key.trim().is_empty() {
            return Err(ProviderError::invalid("Parallel API key is empty"));
        }
        let http = HttpClient::new(config.timeout)?;
        let headers = api_key_header("x-api-key", config.api_key.trim())?;
        Ok(Self {
            http,
            base_url: config.base_url.trim_end_matches('/').to_owned(),
            headers,
            max_results: config.max_results.max(1),
            max_urls: config.max_urls.clamp(1, 20),
        })
    }

    pub(crate) async fn search(
        &self,
        mut request: SearchRequest,
    ) -> Result<SearchResponse, ProviderError> {
        if request.search_queries.is_empty()
            || request
                .search_queries
                .iter()
                .all(|query| query.trim().is_empty())
        {
            return Err(ProviderError::invalid(
                "`search_queries` must contain at least one nonempty keyword query",
            ));
        }
        if let Some(settings) = request.advanced_settings.as_mut() {
            settings.max_results =
                cap_usize(settings.max_results, self.max_results, "max_results")?;
        }
        let url = format!("{}/v1/search", self.base_url);
        self.http
            .post_json(&url, self.headers.clone(), &request)
            .await
    }

    pub(crate) async fn extract(
        &self,
        request: ExtractRequest,
    ) -> Result<ExtractResponse, ProviderError> {
        if request.urls.is_empty() {
            return Err(ProviderError::invalid("`urls` must be nonempty"));
        }
        if request.urls.len() > self.max_urls {
            return Err(ProviderError::invalid(format!(
                "`urls` length {} exceeds the host maximum of {}",
                request.urls.len(),
                self.max_urls
            )));
        }
        let url = format!("{}/v1/extract", self.base_url);
        self.http
            .post_json(&url, self.headers.clone(), &request)
            .await
    }
}
