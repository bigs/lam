use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use lam_core::{
    CompactionArtifact, CompactionConfig, Compactor, EncodedPayload, ModelCodec, ModelDescriptor,
    ModelEventSink, ModelProvider, ModelRequestConfig, ModelResponseMetadata,
    ModelResponseProjection, ProjectedContextEntry,
};

/// One configured model transport and its pure payload codec.
pub struct Model<P, C> {
    pub(crate) provider: Arc<P>,
    pub(crate) codec: Arc<C>,
    descriptor: ModelDescriptor,
    context_window_tokens: Option<u64>,
}

impl<P, C> Clone for Model<P, C> {
    fn clone(&self) -> Self {
        Self {
            provider: Arc::clone(&self.provider),
            codec: Arc::clone(&self.codec),
            descriptor: self.descriptor.clone(),
            context_window_tokens: self.context_window_tokens,
        }
    }
}

impl<P, C> Model<P, C> {
    /// Couples a provider transport with the codec for its native payloads.
    ///
    /// The default durable descriptor uses Rust type names. Provider adapters
    /// should replace it with [`Self::with_descriptor`] so historical actor
    /// logs contain a useful model name.
    #[must_use]
    pub fn new(provider: P, codec: C) -> Self {
        Self {
            provider: Arc::new(provider),
            codec: Arc::new(codec),
            descriptor: ModelDescriptor::new(
                std::any::type_name::<P>(),
                std::any::type_name::<P>(),
                std::any::type_name::<C>(),
            )
            .expect("Rust type names are nonempty"),
            context_window_tokens: None,
        }
    }

    /// Replaces the non-secret identity written to actor journals.
    #[must_use]
    pub fn with_descriptor(mut self, descriptor: ModelDescriptor) -> Self {
        self.descriptor = descriptor;
        self
    }

    /// Declares this model's context window for automatic compaction.
    #[must_use]
    pub const fn with_context_window_tokens(mut self, tokens: u64) -> Self {
        self.context_window_tokens = Some(tokens);
        self
    }

    /// Returns this model's declared context window, when configured.
    #[must_use]
    pub const fn context_window_tokens(&self) -> Option<u64> {
        self.context_window_tokens
    }

    /// Returns the non-secret durable model description.
    #[must_use]
    pub const fn descriptor(&self) -> &ModelDescriptor {
        &self.descriptor
    }

    /// Clones the shared provider and codec for custom compactor composition.
    #[must_use]
    pub fn shared_parts(&self) -> (Arc<P>, Arc<C>) {
        (Arc::clone(&self.provider), Arc::clone(&self.codec))
    }
}

pub(crate) struct RegisteredModel {
    runtime: Arc<dyn RuntimeModel>,
    pub(crate) compactor: Option<Arc<dyn Compactor>>,
}

impl Clone for RegisteredModel {
    fn clone(&self) -> Self {
        Self {
            runtime: Arc::clone(&self.runtime),
            compactor: self.compactor.as_ref().map(Arc::clone),
        }
    }
}

impl RegisteredModel {
    pub(crate) fn new<P, C>(model: Model<P, C>, compactor: Option<Arc<dyn Compactor>>) -> Self
    where
        P: ModelProvider,
        C: ModelCodec,
    {
        Self {
            runtime: Arc::new(RuntimeModelAdapter { model }),
            compactor,
        }
    }

    pub(crate) fn compaction_config(&self, fallback: &CompactionConfig) -> CompactionConfig {
        self.runtime.context_window_tokens().map_or_else(
            || fallback.clone(),
            |tokens| fallback.clone().context_window_tokens(tokens),
        )
    }

    pub(crate) fn descriptor(&self) -> &ModelDescriptor {
        self.runtime.descriptor()
    }

    pub(crate) fn encode_request(
        &self,
        context: &[ProjectedContextEntry],
        config: &ModelRequestConfig<'_>,
    ) -> Result<EncodedPayload, String> {
        self.runtime.encode_request(context, config)
    }

    pub(crate) fn invoke(
        &self,
        request: EncodedPayload,
        events: ModelEventSink,
    ) -> RuntimeModelFuture<'_> {
        self.runtime.invoke(request, events)
    }

    pub(crate) fn project_response(
        &self,
        response: &EncodedPayload,
    ) -> Result<ModelResponseProjection, String> {
        self.runtime.project_response(response)
    }

    pub(crate) fn response_metadata(&self, response: &EncodedPayload) -> ModelResponseMetadata {
        self.runtime.response_metadata(response)
    }

    pub(crate) fn materialize_compaction(
        &self,
        artifact: &CompactionArtifact,
    ) -> Result<Option<EncodedPayload>, String> {
        self.runtime.materialize_compaction(artifact)
    }

    pub(crate) fn accepts_compaction_replacement(&self, replacement: &EncodedPayload) -> bool {
        self.runtime.accepts_compaction_replacement(replacement)
    }
}

pub(crate) struct RuntimeProviderError {
    pub(crate) message: String,
    pub(crate) context_overflow: bool,
}

pub(crate) type RuntimeModelFuture<'a> =
    Pin<Box<dyn Future<Output = Result<EncodedPayload, RuntimeProviderError>> + Send + 'a>>;

trait RuntimeModel: Send + Sync {
    fn descriptor(&self) -> &ModelDescriptor;

    fn context_window_tokens(&self) -> Option<u64>;

    fn encode_request(
        &self,
        context: &[ProjectedContextEntry],
        config: &ModelRequestConfig<'_>,
    ) -> Result<EncodedPayload, String>;

    fn invoke(&self, request: EncodedPayload, events: ModelEventSink) -> RuntimeModelFuture<'_>;

    fn project_response(
        &self,
        response: &EncodedPayload,
    ) -> Result<ModelResponseProjection, String>;

    fn response_metadata(&self, response: &EncodedPayload) -> ModelResponseMetadata;

    fn materialize_compaction(
        &self,
        artifact: &CompactionArtifact,
    ) -> Result<Option<EncodedPayload>, String>;

    fn accepts_compaction_replacement(&self, replacement: &EncodedPayload) -> bool;
}

struct RuntimeModelAdapter<P, C> {
    model: Model<P, C>,
}

impl<P, C> RuntimeModel for RuntimeModelAdapter<P, C>
where
    P: ModelProvider,
    C: ModelCodec,
{
    fn descriptor(&self) -> &ModelDescriptor {
        self.model.descriptor()
    }

    fn context_window_tokens(&self) -> Option<u64> {
        self.model.context_window_tokens()
    }

    fn encode_request(
        &self,
        context: &[ProjectedContextEntry],
        config: &ModelRequestConfig<'_>,
    ) -> Result<EncodedPayload, String> {
        self.model
            .codec
            .encode_request(context, config)
            .map_err(|error| error.to_string())
    }

    fn invoke(&self, request: EncodedPayload, events: ModelEventSink) -> RuntimeModelFuture<'_> {
        Box::pin(async move {
            self.model
                .provider
                .invoke(request, events)
                .await
                .map_err(|error| RuntimeProviderError {
                    context_overflow: self.model.provider.is_context_overflow(&error),
                    message: error.to_string(),
                })
        })
    }

    fn project_response(
        &self,
        response: &EncodedPayload,
    ) -> Result<ModelResponseProjection, String> {
        self.model
            .codec
            .project_response(response)
            .map_err(|error| error.to_string())
    }

    fn response_metadata(&self, response: &EncodedPayload) -> ModelResponseMetadata {
        self.model.codec.response_metadata(response)
    }

    fn materialize_compaction(
        &self,
        artifact: &CompactionArtifact,
    ) -> Result<Option<EncodedPayload>, String> {
        self.model
            .codec
            .materialize_compaction(artifact)
            .map_err(|error| error.to_string())
    }

    fn accepts_compaction_replacement(&self, replacement: &EncodedPayload) -> bool {
        self.model.codec.accepts_compaction_replacement(replacement)
    }
}
