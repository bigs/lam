use crate::error::ProviderError;
use crate::exa::config::ExaConfig;
use crate::exa::types::{
    AnswerRequest, AnswerResponse, ContentsRequest, ContentsResponse, ContextRequest,
    ContextResponse, FindSimilarRequest, SearchRequest, SearchResponse,
};
use crate::http::{HttpClient, api_key_header, cap_usize};

#[derive(Clone, Debug)]
pub(crate) struct ExaClient {
    http: HttpClient,
    base_url: String,
    headers: reqwest::header::HeaderMap,
    max_results: usize,
    max_urls: usize,
}

impl ExaClient {
    pub(crate) fn new(config: &ExaConfig) -> Result<Self, ProviderError> {
        if config.api_key.trim().is_empty() {
            return Err(ProviderError::invalid("Exa API key is empty"));
        }
        let http = HttpClient::new(config.timeout)?;
        let headers = api_key_header("x-api-key", config.api_key.trim())?;
        Ok(Self {
            http,
            base_url: config.base_url.trim_end_matches('/').to_owned(),
            headers,
            max_results: config.max_results.max(1),
            max_urls: config.max_urls.max(1),
        })
    }

    pub(crate) async fn search(
        &self,
        mut request: SearchRequest,
    ) -> Result<SearchResponse, ProviderError> {
        if request.query.trim().is_empty() {
            return Err(ProviderError::invalid("`query` must be nonempty"));
        }
        request.num_results = cap_usize(request.num_results, self.max_results, "numResults")?;
        // Streaming is not supported through the isolate bridge.
        let url = format!("{}/search", self.base_url);
        self.http
            .post_json(&url, self.headers.clone(), &request)
            .await
    }

    pub(crate) async fn contents(
        &self,
        request: ContentsRequest,
    ) -> Result<ContentsResponse, ProviderError> {
        let urls = request.urls.as_deref().unwrap_or(&[]);
        let ids = request.ids.as_deref().unwrap_or(&[]);
        match (urls.is_empty(), ids.is_empty()) {
            (true, true) => {
                return Err(ProviderError::invalid(
                    "provide either `urls` or `ids` (not both empty)",
                ));
            }
            (false, false) => {
                return Err(ProviderError::invalid(
                    "provide either `urls` or `ids`, not both",
                ));
            }
            (false, true) if urls.len() > self.max_urls => {
                return Err(ProviderError::invalid(format!(
                    "`urls` length {} exceeds the host maximum of {}",
                    urls.len(),
                    self.max_urls
                )));
            }
            (true, false) if ids.len() > self.max_urls => {
                return Err(ProviderError::invalid(format!(
                    "`ids` length {} exceeds the host maximum of {}",
                    ids.len(),
                    self.max_urls
                )));
            }
            _ => {}
        }
        // Default text:true when no content options set, so a bare contents call is useful.
        let mut body = request;
        if body.text.is_none()
            && body.highlights.is_none()
            && body.summary.is_none()
            && body.extras.is_none()
        {
            body.text = Some(serde_json::Value::Bool(true));
        }
        let url = format!("{}/contents", self.base_url);
        self.http.post_json(&url, self.headers.clone(), &body).await
    }

    pub(crate) async fn context(
        &self,
        request: ContextRequest,
    ) -> Result<ContextResponse, ProviderError> {
        if request.query.trim().is_empty() {
            return Err(ProviderError::invalid("`query` must be nonempty"));
        }
        let url = format!("{}/context", self.base_url);
        self.http
            .post_json(&url, self.headers.clone(), &request)
            .await
    }

    pub(crate) async fn answer(
        &self,
        request: AnswerRequest,
    ) -> Result<AnswerResponse, ProviderError> {
        if request.query.trim().is_empty() {
            return Err(ProviderError::invalid("`query` must be nonempty"));
        }
        let url = format!("{}/answer", self.base_url);
        self.http
            .post_json(&url, self.headers.clone(), &request)
            .await
    }

    pub(crate) async fn find_similar(
        &self,
        mut request: FindSimilarRequest,
    ) -> Result<SearchResponse, ProviderError> {
        if request.url.trim().is_empty() {
            return Err(ProviderError::invalid("`url` must be nonempty"));
        }
        request.num_results = cap_usize(request.num_results, self.max_results, "numResults")?;
        let url = format!("{}/findSimilar", self.base_url);
        self.http
            .post_json(&url, self.headers.clone(), &request)
            .await
    }
}
