use std::sync::Arc;

/// One configured model transport and its pure payload codec.
pub struct Model<P, C> {
    pub(crate) provider: Arc<P>,
    pub(crate) codec: Arc<C>,
}

impl<P, C> Model<P, C> {
    /// Couples a provider transport with the codec for its native payloads.
    #[must_use]
    pub fn new(provider: P, codec: C) -> Self {
        Self {
            provider: Arc::new(provider),
            codec: Arc::new(codec),
        }
    }
}
