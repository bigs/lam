use std::future::Future;
use std::pin::Pin;

use serde::{Deserialize, Serialize};

use crate::{
    CodecId, CodecRef, ContextSequence, ContextTransition, EncodedPayload, ModelDescriptor,
    ModelResponseMetadata, ProjectedContextEntry, RunProgress,
};

/// Codec used by Lam's durable compaction records.
pub const COMPACTION_RECORD_CODEC_ID: &str = "lam/compaction";

/// Current durable compaction-record representation.
pub const COMPACTION_RECORD_CODEC_VERSION: u32 = 2;

/// One context-relative token amount.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ContextAmount {
    /// An absolute token count.
    Tokens(u64),
    /// A fraction of the model context window in the range `(0, 1]`.
    Ratio(f64),
}

impl ContextAmount {
    /// Resolves this amount against a concrete context window.
    pub fn resolve(self, context_window_tokens: u64) -> Result<u64, CompactionConfigError> {
        if context_window_tokens == 0 {
            return Err(CompactionConfigError::EmptyContextWindow);
        }
        match self {
            Self::Tokens(tokens) => Ok(tokens.min(context_window_tokens)),
            Self::Ratio(ratio) if ratio.is_finite() && ratio > 0.0 && ratio <= 1.0 => {
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let tokens = ((context_window_tokens as f64) * ratio).floor() as u64;
                Ok(tokens.max(1).min(context_window_tokens))
            }
            Self::Ratio(ratio) => Err(CompactionConfigError::InvalidRatio { ratio }),
        }
    }
}

/// Automatic compaction thresholds and budgets.
#[derive(Clone, Debug, PartialEq)]
pub struct CompactionConfig {
    context_window_tokens: Option<u64>,
    trigger_at: ContextAmount,
    retain: ContextAmount,
    summary_reserve_tokens: u64,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            context_window_tokens: None,
            trigger_at: ContextAmount::Ratio(0.9),
            retain: ContextAmount::Tokens(20_000),
            summary_reserve_tokens: 8_192,
        }
    }
}

impl CompactionConfig {
    /// Declares the selected model's context window.
    ///
    /// Automatic threshold checks are inactive until a window is configured.
    #[must_use]
    pub const fn context_window_tokens(mut self, tokens: u64) -> Self {
        self.context_window_tokens = Some(tokens);
        self
    }

    /// Chooses the amount of consumed context which triggers compaction.
    #[must_use]
    pub const fn trigger_at(mut self, amount: ContextAmount) -> Self {
        self.trigger_at = amount;
        self
    }

    /// Chooses the target amount of exact recent context retained after compaction.
    #[must_use]
    pub const fn retain(mut self, amount: ContextAmount) -> Self {
        self.retain = amount;
        self
    }

    /// Reserves output capacity for a model-generated summary.
    #[must_use]
    pub const fn summary_reserve_tokens(mut self, tokens: u64) -> Self {
        self.summary_reserve_tokens = tokens;
        self
    }

    /// Returns the configured model context window.
    #[must_use]
    pub const fn context_window(&self) -> Option<u64> {
        self.context_window_tokens
    }

    /// Returns the configured trigger amount.
    #[must_use]
    pub const fn trigger(&self) -> ContextAmount {
        self.trigger_at
    }

    /// Returns the configured retained-tail target.
    #[must_use]
    pub const fn retained_context(&self) -> ContextAmount {
        self.retain
    }

    /// Returns the summary-output reserve.
    #[must_use]
    pub const fn summary_reserve(&self) -> u64 {
        self.summary_reserve_tokens
    }

    /// Resolves the effective automatic trigger.
    ///
    /// The summary reserve is a hard safety bound even when `trigger_at` would
    /// otherwise allow context to grow later.
    pub fn automatic_trigger_tokens(&self) -> Result<Option<u64>, CompactionConfigError> {
        let Some(window) = self.context_window_tokens else {
            return Ok(None);
        };
        self.validate()?;
        self.effective_trigger(window).map(Some)
    }

    /// Resolves the retained-tail target.
    pub fn retain_tokens(&self) -> Result<u64, CompactionConfigError> {
        match self.context_window_tokens {
            Some(window) => self.retain.resolve(window),
            None => match self.retain {
                ContextAmount::Tokens(tokens) => Ok(tokens),
                ContextAmount::Ratio(_) => Err(CompactionConfigError::RatioWithoutContextWindow),
            },
        }
    }

    /// Validates internally related settings.
    pub fn validate(&self) -> Result<(), CompactionConfigError> {
        if self.summary_reserve_tokens == 0 {
            return Err(CompactionConfigError::EmptySummaryReserve);
        }
        validate_ratio(self.trigger_at)?;
        validate_ratio(self.retain)?;
        if let Some(window) = self.context_window_tokens {
            if window == 0 {
                return Err(CompactionConfigError::EmptyContextWindow);
            }
            if self.summary_reserve_tokens >= window {
                return Err(CompactionConfigError::SummaryReserveExhaustsContext {
                    reserve: self.summary_reserve_tokens,
                    context_window: window,
                });
            }
            let trigger = self.effective_trigger(window)?;
            let retain = self.retain.resolve(window)?;
            if retain >= trigger {
                return Err(CompactionConfigError::RetainedTailReachesTrigger { retain, trigger });
            }
        } else if matches!(self.retain, ContextAmount::Ratio(_)) {
            return Err(CompactionConfigError::RatioWithoutContextWindow);
        }
        Ok(())
    }

    fn effective_trigger(&self, window: u64) -> Result<u64, CompactionConfigError> {
        Ok(self
            .trigger_at
            .resolve(window)?
            .min(window.saturating_sub(self.summary_reserve_tokens)))
    }
}

/// Invalid compaction configuration.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum CompactionConfigError {
    /// A context window must contain at least one token.
    #[error("model context window must be greater than zero")]
    EmptyContextWindow,
    /// A ratio was non-finite or outside `(0, 1]`.
    #[error("context ratio must be finite and in (0, 1], received {ratio}")]
    InvalidRatio {
        /// Invalid ratio.
        ratio: f64,
    },
    /// A relative retained-tail target lacks a model context window.
    #[error("a ratio-based retained tail requires a configured context window")]
    RatioWithoutContextWindow,
    /// Summary generation requires nonzero output capacity.
    #[error("summary reserve must be greater than zero")]
    EmptySummaryReserve,
    /// Summary output would consume the entire declared window.
    #[error("summary reserve {reserve} must be smaller than context window {context_window}")]
    SummaryReserveExhaustsContext {
        /// Configured reserve.
        reserve: u64,
        /// Declared context window.
        context_window: u64,
    },
    /// The requested exact tail cannot fit below the automatic trigger.
    #[error("retained tail {retain} must be smaller than effective trigger {trigger}")]
    RetainedTailReachesTrigger {
        /// Resolved retained-tail target.
        retain: u64,
        /// Resolved trigger after reserving summary capacity.
        trigger: u64,
    },
}

/// Why one compaction was requested.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum CompactionReason {
    /// An embedding explicitly requested compaction.
    Manual,
    /// Estimated context crossed the configured threshold.
    Threshold,
    /// A provider rejected an inference because its context was too large.
    Overflow,
    /// A model switch requested context prepared for the target model.
    ModelSwitch,
}

/// Portable model-visible result of Lam-managed compaction.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionArtifact {
    /// Concise state needed to continue the conversation.
    pub summary: String,
    /// Optional exact excerpts selected by a compactor.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub excerpts: Vec<String>,
}

impl CompactionArtifact {
    /// Constructs a summary without exact excerpts.
    #[must_use]
    pub fn summary(summary: impl Into<String>) -> Self {
        Self {
            summary: summary.into(),
            excerpts: Vec::new(),
        }
    }

    /// Returns whether this artifact can provide a useful replacement view.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.summary.trim().is_empty() && self.excerpts.iter().all(|value| value.trim().is_empty())
    }
}

/// Durable provenance and exact model-visible output of one compaction.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionRecord {
    /// Stable diagnostic name of the selected compactor.
    pub strategy: String,
    /// Trigger which requested this compaction.
    pub reason: CompactionReason,
    /// Untouched provider response used to derive this checkpoint, when applicable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<EncodedPayload>,
    /// Optional provider-neutral summary and excerpts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact: Option<CompactionArtifact>,
    /// Exact codec-specific context item replayed to the model.
    pub replacement: EncodedPayload,
    /// Normalized usage and cost from compaction inference.
    #[serde(default)]
    pub metadata: ModelResponseMetadata,
}

impl CompactionRecord {
    /// Encodes this record as one authoritative context payload.
    pub fn encode(&self) -> Result<EncodedPayload, serde_json::Error> {
        Ok(EncodedPayload::new(
            compaction_record_codec(),
            serde_json::to_value(self)?,
        ))
    }

    /// Decodes a Lam compaction payload.
    pub fn decode(payload: &EncodedPayload) -> Result<Option<Self>, serde_json::Error> {
        if payload.codec.id.as_str() != COMPACTION_RECORD_CODEC_ID
            || !(1..=COMPACTION_RECORD_CODEC_VERSION).contains(&payload.codec.version)
        {
            return Ok(None);
        }
        serde_json::from_value(payload.value.clone()).map(Some)
    }
}

/// One indivisible span of context for compaction cut selection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompactionUnit {
    entries: Vec<ProjectedContextEntry>,
    estimated_tokens: u64,
}

impl CompactionUnit {
    /// Returns the entries which must be retained or summarized together.
    #[must_use]
    pub fn entries(&self) -> &[ProjectedContextEntry] {
        &self.entries
    }

    /// Returns the estimated token weight of this unit.
    #[must_use]
    pub const fn estimated_tokens(&self) -> u64 {
        self.estimated_tokens
    }

    /// Returns the unit's inclusive context boundary.
    #[must_use]
    pub fn covers_through(&self) -> ContextSequence {
        self.entries
            .last()
            .expect("compaction units are nonempty")
            .sequence
    }
}

/// Groups context so a model continuation and its eval outcome cannot be split.
#[must_use]
pub fn atomic_compaction_units(context: &[ProjectedContextEntry]) -> Vec<CompactionUnit> {
    let mut units: Vec<CompactionUnit> = Vec::new();
    for entry in context {
        let joins_previous = units.last().is_some_and(|unit| {
            let Some(previous) = unit.entries.last() else {
                return false;
            };
            matches!(
                previous.entry.transition,
                ContextTransition::Model {
                    progress: RunProgress::Continue,
                    ..
                }
            ) && matches!(
                entry.entry.transition,
                ContextTransition::Eval { .. } | ContextTransition::Messages { .. }
            ) && previous.entry.transition.run_id() == entry.entry.transition.run_id()
        });
        let estimated = estimate_entry_tokens(entry);
        if joins_previous {
            let unit = units.last_mut().expect("the previous unit was checked");
            unit.entries.push(entry.clone());
            unit.estimated_tokens = unit.estimated_tokens.saturating_add(estimated);
        } else {
            units.push(CompactionUnit {
                entries: vec![entry.clone()],
                estimated_tokens: estimated,
            });
        }
    }
    units
}

/// Estimates one projected context entry using a deliberately cheap fallback.
#[must_use]
pub fn estimate_entry_tokens(entry: &ProjectedContextEntry) -> u64 {
    let characters = entry.entry.payload.value.to_string().chars().count();
    let characters = u64::try_from(characters).unwrap_or(u64::MAX);
    characters.saturating_add(3) / 4 + 8
}

/// Immutable input supplied to one configured compactor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompactionRequest {
    /// Trigger requesting this operation.
    pub reason: CompactionReason,
    /// Complete effective model-visible context at the compaction snapshot.
    pub context: Vec<ProjectedContextEntry>,
    /// Ephemeral instructions active for that context.
    pub instructions: String,
    /// Target selected by a model-switch compaction, when present.
    pub target_model: Option<ModelDescriptor>,
    /// Ordered atomic context units not already covered by the previous artifact.
    pub units: Vec<CompactionUnit>,
    /// Previously accumulated neutral summary, when compacting repeatedly.
    pub previous: Option<CompactionArtifact>,
    /// Target exact-tail size after compaction.
    pub retain_tokens: u64,
    /// Maximum summary output requested from a model-backed compactor.
    pub max_output_tokens: u64,
}

/// Model-visible value proposed by one compactor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CompactionOutput {
    /// Portable state which the target codec must materialize.
    Artifact(CompactionArtifact),
    /// Exact provider-native state which must be replayed unchanged.
    Exact {
        /// Canonical codec-specific replacement.
        replacement: EncodedPayload,
        /// Optional portable companion for inspection or later conversion.
        artifact: Option<CompactionArtifact>,
    },
}

impl CompactionOutput {
    /// Constructs an exact replacement without inventing a portable view.
    #[must_use]
    pub const fn exact(replacement: EncodedPayload) -> Self {
        Self::Exact {
            replacement,
            artifact: None,
        }
    }
}

/// Successful proposal returned to the actor engine for validation and commit.
#[derive(Clone, Debug, PartialEq)]
pub struct CompactionPlan {
    /// Stable diagnostic name of the strategy which produced this plan.
    pub strategy: String,
    /// Inclusive raw context boundary replaced by the artifact.
    pub covers_through: ContextSequence,
    /// Portable or exact model-visible output produced by the compactor.
    pub output: CompactionOutput,
    /// Untouched provider response used to derive the checkpoint.
    pub source: Option<EncodedPayload>,
    /// Normalized compaction-inference usage and cost.
    pub metadata: ModelResponseMetadata,
}

/// A compactor could not produce a valid checkpoint proposal.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[error("{message}")]
pub struct CompactionError {
    message: String,
}

impl CompactionError {
    /// Constructs an implementation-supplied compaction diagnostic.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

/// Boxed future returned by dynamically configured compactors.
pub type CompactionFuture<'a> =
    Pin<Box<dyn Future<Output = Result<CompactionPlan, CompactionError>> + Send + 'a>>;

/// Replaceable strategy for converting an old context prefix into one artifact.
pub trait Compactor: Send + Sync + 'static {
    /// Proposes one compaction without mutating actor state.
    fn compact<'a>(&'a self, request: &'a CompactionRequest) -> CompactionFuture<'a>;
}

/// Selects how many leading units should be summarized for a retained-tail target.
///
/// A newest unit which alone exceeds the target is summarized whole rather
/// than being sliced into an invalid tool or message fragment.
#[must_use]
pub fn compaction_prefix_len(units: &[CompactionUnit], retain_tokens: u64) -> Option<usize> {
    let newest = units.last()?;
    if newest.estimated_tokens() > retain_tokens {
        return Some(units.len());
    }

    let mut retained = 0_u64;
    let mut keep_from = units.len();
    for (index, unit) in units.iter().enumerate().rev() {
        if retained >= retain_tokens {
            break;
        }
        retained = retained.saturating_add(unit.estimated_tokens());
        keep_from = index;
    }
    (keep_from > 0).then_some(keep_from)
}

fn compaction_record_codec() -> CodecRef {
    CodecRef::new(
        CodecId::new(COMPACTION_RECORD_CODEC_ID).expect("Lam's compaction codec id is valid"),
        COMPACTION_RECORD_CODEC_VERSION,
    )
}

fn validate_ratio(amount: ContextAmount) -> Result<(), CompactionConfigError> {
    if let ContextAmount::Ratio(ratio) = amount
        && (!ratio.is_finite() || ratio <= 0.0 || ratio > 1.0)
    {
        return Err(CompactionConfigError::InvalidRatio { ratio });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::{ContextEntry, Revision, RunId, Timestamp};

    fn projected(
        sequence: u64,
        transition: ContextTransition,
        value: &str,
    ) -> ProjectedContextEntry {
        ProjectedContextEntry {
            sequence: ContextSequence::new(sequence),
            revision: Revision::new(sequence),
            entry: ContextEntry {
                transition,
                payload: EncodedPayload::lam_json(value).expect("fixture should encode"),
                recorded_at: Timestamp::from_unix_millis(0),
            },
        }
    }

    #[test]
    fn threshold_respects_summary_reserve() {
        let config = CompactionConfig::default()
            .context_window_tokens(100_000)
            .trigger_at(ContextAmount::Ratio(0.95))
            .summary_reserve_tokens(10_000);
        assert_eq!(config.automatic_trigger_tokens().unwrap(), Some(90_000));
    }

    #[test]
    fn relative_tail_requires_a_declared_context_window() {
        let config = CompactionConfig::default().retain(ContextAmount::Ratio(0.2));
        assert_eq!(
            config.validate(),
            Err(CompactionConfigError::RatioWithoutContextWindow)
        );
    }

    #[test]
    fn retained_tail_must_fit_below_the_effective_trigger() {
        let config = CompactionConfig::default()
            .context_window_tokens(1_000)
            .trigger_at(ContextAmount::Tokens(800))
            .retain(ContextAmount::Tokens(800))
            .summary_reserve_tokens(100);
        assert_eq!(
            config.validate(),
            Err(CompactionConfigError::RetainedTailReachesTrigger {
                retain: 800,
                trigger: 800,
            })
        );
    }

    #[test]
    fn continuation_and_eval_are_one_atomic_unit() {
        let run = RunId::new("run").unwrap();
        let context = vec![
            projected(
                1,
                ContextTransition::Model {
                    run_id: run.clone(),
                    progress: RunProgress::Continue,
                },
                "call",
            ),
            projected(2, ContextTransition::Eval { run_id: run }, "result"),
        ];
        let units = atomic_compaction_units(&context);
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].entries().len(), 2);
        assert_eq!(units[0].covers_through(), ContextSequence::new(2));
    }

    #[test]
    fn oversized_newest_unit_is_summarized_whole() {
        let run = RunId::new("run").unwrap();
        let context = vec![projected(
            1,
            ContextTransition::Messages {
                run_id: run,
                consumed_message_ids: Vec::new(),
            },
            &"x".repeat(100),
        )];
        let units = atomic_compaction_units(&context);
        assert_eq!(compaction_prefix_len(&units, 1), Some(1));
    }

    #[test]
    fn compaction_record_round_trips_with_exact_replacement() {
        let replacement =
            EncodedPayload::lam_json(json!({ "role": "user", "text": "state" })).unwrap();
        let record = CompactionRecord {
            strategy: "summary-tail".to_owned(),
            reason: CompactionReason::Threshold,
            source: None,
            artifact: Some(CompactionArtifact::summary("state")),
            replacement: replacement.clone(),
            metadata: ModelResponseMetadata::default(),
        };
        let encoded = record.encode().unwrap();
        let decoded = CompactionRecord::decode(&encoded).unwrap().unwrap();
        assert_eq!(decoded.replacement, replacement);
        assert_eq!(decoded, record);
    }

    #[test]
    fn version_one_compaction_record_remains_readable() {
        let replacement =
            EncodedPayload::lam_json(json!({ "role": "user", "text": "legacy state" })).unwrap();
        let encoded = EncodedPayload::new(
            CodecRef::new(CodecId::new(COMPACTION_RECORD_CODEC_ID).unwrap(), 1),
            json!({
                "strategy": "summary-tail",
                "reason": "manual",
                "artifact": {
                    "summary": "legacy state",
                    "excerpts": []
                },
                "replacement": replacement
            }),
        );

        let decoded = CompactionRecord::decode(&encoded).unwrap().unwrap();
        assert_eq!(decoded.strategy, "summary-tail");
        assert_eq!(decoded.reason, CompactionReason::Manual);
        assert_eq!(decoded.artifact.unwrap().summary, "legacy state");
        assert_eq!(decoded.replacement, replacement);
        assert_eq!(decoded.metadata, ModelResponseMetadata::default());
    }
}
