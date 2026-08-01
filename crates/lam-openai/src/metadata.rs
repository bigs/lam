use lam::{ModelCost, ModelCostSource, ModelResponseMetadata, TokenUsage};
use serde_json::Value;

/// USD prices per million tokens for one model and service tier.
///
/// Prices are supplied by the embedding because provider catalogs and service
/// tiers change independently of Lam releases.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ModelPricing {
    input_usd_per_million: f64,
    cached_input_usd_per_million: Option<f64>,
    output_usd_per_million: f64,
}

impl ModelPricing {
    /// Creates pricing for uncached input and generated output tokens.
    #[must_use]
    pub const fn new(input_usd_per_million: f64, output_usd_per_million: f64) -> Self {
        Self {
            input_usd_per_million,
            cached_input_usd_per_million: None,
            output_usd_per_million,
        }
    }

    /// Sets the discounted price for cached input tokens.
    #[must_use]
    pub const fn cached_input(mut self, usd_per_million: f64) -> Self {
        self.cached_input_usd_per_million = Some(usd_per_million);
        self
    }

    /// Returns the configured uncached-input price.
    #[must_use]
    pub const fn input_usd_per_million(self) -> f64 {
        self.input_usd_per_million
    }

    /// Returns the configured cached-input price, when distinct.
    #[must_use]
    pub const fn cached_input_usd_per_million(self) -> Option<f64> {
        self.cached_input_usd_per_million
    }

    /// Returns the configured output price.
    #[must_use]
    pub const fn output_usd_per_million(self) -> f64 {
        self.output_usd_per_million
    }

    pub(crate) fn is_valid(self) -> bool {
        valid_rate(self.input_usd_per_million)
            && self.cached_input_usd_per_million.is_none_or(valid_rate)
            && valid_rate(self.output_usd_per_million)
    }

    fn estimate(self, usage: &TokenUsage) -> ModelCost {
        let cached = usage
            .cached_input_tokens
            .unwrap_or_default()
            .min(usage.input_tokens);
        let uncached = usage.input_tokens - cached;
        let cached_rate = self
            .cached_input_usd_per_million
            .unwrap_or(self.input_usd_per_million);
        let amount_usd = (uncached as f64 * self.input_usd_per_million
            + cached as f64 * cached_rate
            + usage.output_tokens as f64 * self.output_usd_per_million)
            / 1_000_000.0;
        ModelCost {
            amount_usd: round_usd(amount_usd),
            source: ModelCostSource::Estimated,
        }
    }
}

fn valid_rate(rate: f64) -> bool {
    rate.is_finite() && rate >= 0.0
}

fn round_usd(amount: f64) -> f64 {
    // Keep sub-picodollar binary floating-point noise out of events and logs.
    const PLACES: f64 = 1_000_000_000_000.0;
    (amount * PLACES).round() / PLACES
}

#[derive(Clone, Copy)]
pub(crate) enum UsageDialect {
    Responses,
    ChatCompletions,
}

pub(crate) fn response_metadata(
    model: String,
    native_usage: Option<&Value>,
    dialect: UsageDialect,
    pricing: Option<ModelPricing>,
) -> ModelResponseMetadata {
    let usage = native_usage.and_then(|usage| normalize_usage(usage, dialect));
    let cost = usage
        .as_ref()
        .and_then(|usage| pricing.map(|pricing| pricing.estimate(usage)));
    ModelResponseMetadata {
        model: Some(model),
        usage,
        cost,
    }
}

fn normalize_usage(native: &Value, dialect: UsageDialect) -> Option<TokenUsage> {
    let (input, output, input_details, output_details) = match dialect {
        UsageDialect::Responses => (
            "input_tokens",
            "output_tokens",
            "input_tokens_details",
            "output_tokens_details",
        ),
        UsageDialect::ChatCompletions => (
            "prompt_tokens",
            "completion_tokens",
            "prompt_tokens_details",
            "completion_tokens_details",
        ),
    };
    let input_tokens = native.get(input)?.as_u64()?;
    let output_tokens = native.get(output)?.as_u64()?;
    let total_tokens = native
        .get("total_tokens")
        .and_then(Value::as_u64)
        .unwrap_or_else(|| input_tokens.saturating_add(output_tokens));
    let cached_input_tokens = detail(native, input_details, "cached_tokens");
    let reasoning_tokens = detail(native, output_details, "reasoning_tokens");
    Some(TokenUsage {
        input_tokens,
        cached_input_tokens,
        output_tokens,
        reasoning_tokens,
        total_tokens,
        native: native.clone(),
    })
}

fn detail(usage: &Value, group: &str, field: &str) -> Option<u64> {
    usage.get(group)?.get(field)?.as_u64()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn estimates_cached_and_uncached_tokens_separately() {
        let native = json!({
            "prompt_tokens": 1_000,
            "prompt_tokens_details": { "cached_tokens": 800 },
            "completion_tokens": 100,
            "completion_tokens_details": { "reasoning_tokens": 60 },
            "total_tokens": 1_100,
            "provider_extension": { "kept": true }
        });
        let metadata = response_metadata(
            "test-model".to_owned(),
            Some(&native),
            UsageDialect::ChatCompletions,
            Some(ModelPricing::new(0.14, 0.28).cached_input(0.028)),
        );
        let usage = metadata.usage.expect("usage is normalized");
        assert_eq!(usage.cached_input_tokens, Some(800));
        assert_eq!(usage.reasoning_tokens, Some(60));
        assert_eq!(usage.native, native);
        let cost = metadata.cost.expect("cost is estimated");
        assert_eq!(cost.source, ModelCostSource::Estimated);
        assert!((cost.amount_usd - 0.000_078_4).abs() < f64::EPSILON);
    }

    #[test]
    fn unfamiliar_usage_never_blocks_the_response() {
        let metadata = response_metadata(
            "test-model".to_owned(),
            Some(&json!({ "future_tokens": 12 })),
            UsageDialect::Responses,
            Some(ModelPricing::new(1.0, 2.0)),
        );
        assert!(metadata.usage.is_none());
        assert!(metadata.cost.is_none());
    }

    #[test]
    fn removes_binary_float_noise_from_observable_costs() {
        let rounded = round_usd(0.000_052_948_000_000_000_01);
        assert_eq!(rounded, 0.000_052_948);
        assert_eq!(serde_json::to_string(&rounded).unwrap(), "0.000052948");
    }
}
