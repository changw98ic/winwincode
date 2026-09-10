// SPDX-License-Identifier: Apache-2.0

//! Versioned cost projections over immutable model Usage facts.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use winwincode_domain::{ExecutionJobId, ProductSessionId};

use crate::{ModelUsageSourceEntry, ProviderModelSelector};

const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const TOKENS_PER_MILLION: u128 = 1_000_000;

/// Pricing identity. Costs with different identities are never added together.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelPriceReference {
    pub source: String,
    pub version: String,
    pub currency: String,
    pub billing_semantics: String,
}

/// Per-million-token rates for mutually exclusive token categories.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelTokenPrice {
    pub input: u64,
    pub cached_input: u64,
    pub cache_write_input: u64,
    pub output: u64,
    pub reasoning_output: u64,
}

/// One versioned price entry.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelPriceEntry {
    pub model: ProviderModelSelector,
    pub reference: ModelPriceReference,
    pub micros_per_million_tokens: ModelTokenPrice,
}

/// Immutable price catalog used for one recalculation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelPriceCatalog {
    entries: BTreeMap<ProviderModelSelector, ModelPriceEntry>,
}

impl ModelPriceCatalog {
    /// Builds a catalog without duplicate models or unversioned price sources.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed or duplicate entries.
    pub fn try_new(entries: Vec<ModelPriceEntry>) -> Result<Self, ModelCostError> {
        let mut indexed = BTreeMap::new();
        for entry in entries {
            validate_token(&entry.model.provider_id, 128)?;
            validate_token(&entry.model.model_id, 200)?;
            validate_reference(&entry.reference)?;
            if indexed.insert(entry.model.clone(), entry).is_some() {
                return Err(ModelCostError);
            }
        }
        Ok(Self { entries: indexed })
    }

    /// Recalculates one immutable Usage fact; absent pricing remains unknown.
    ///
    /// # Errors
    ///
    /// Returns an error when token categories or calculated cost are invalid.
    pub fn project(
        &self,
        entry: &ModelUsageSourceEntry,
    ) -> Result<ModelCostProjection, ModelCostError> {
        let selector = ProviderModelSelector {
            provider_id: entry.usage.provider_id.clone(),
            model_id: entry.usage.model_id.clone(),
        };
        let class = ModelCostClass::from_profile(&entry.attribution.execution_profile);
        let cost = self
            .entries
            .get(&selector)
            .map_or(Ok(ModelCostFact::Unknown), |price| {
                calculate_cost(&entry.usage, price).map(|amount_micros| ModelCostFact::Known {
                    amount_micros,
                    reference: price.reference.clone(),
                })
            })?;
        Ok(ModelCostProjection {
            provider_usage_id: entry.usage.provider_usage_id.clone(),
            execution_job_id: entry.attribution.execution_job_id.clone(),
            product_session_id: entry.attribution.product_session_id.clone(),
            class,
            cost,
            provider_reported_cost_micros: entry.usage.cost_micros,
        })
    }
}

/// Whether cost belongs to ordinary execution or independent verification.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelCostClass {
    Execution,
    Verification,
}

impl ModelCostClass {
    fn from_profile(profile: &str) -> Self {
        if matches!(profile, "reviewer" | "verifier" | "adversarial-verifier") {
            Self::Verification
        } else {
            Self::Execution
        }
    }
}

/// Derived cost. Unknown is never represented as zero.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModelCostFact {
    Known {
        amount_micros: u64,
        reference: ModelPriceReference,
    },
    Unknown,
}

/// One source Usage fact projected under one retained price catalog.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelCostProjection {
    pub provider_usage_id: String,
    pub execution_job_id: ExecutionJobId,
    pub product_session_id: ProductSessionId,
    pub class: ModelCostClass,
    pub cost: ModelCostFact,
    /// Provider-reported value retained for audit, never mixed with catalog pricing.
    pub provider_reported_cost_micros: u64,
}

/// Cost grouped under one exact price source/version/semantic identity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelCostBucket {
    pub reference: ModelPriceReference,
    pub amount_micros: u64,
}

/// Task and Session cost facts. Verification remains a separate ledger.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ModelCostSummary {
    pub execution_by_task: BTreeMap<String, Vec<ModelCostBucket>>,
    pub verification_by_task: BTreeMap<String, Vec<ModelCostBucket>>,
    pub execution_by_session: BTreeMap<String, Vec<ModelCostBucket>>,
    pub verification_by_session: BTreeMap<String, Vec<ModelCostBucket>>,
    pub unknown_usage_ids: Vec<String>,
}

/// Projects and groups Usage without combining unlike billing semantics.
///
/// # Errors
///
/// Returns an error for invalid Usage arithmetic or cost overflow.
pub fn summarize_model_costs(
    entries: &[ModelUsageSourceEntry],
    catalog: &ModelPriceCatalog,
) -> Result<ModelCostSummary, ModelCostError> {
    let mut summary = ModelCostSummary::default();
    for entry in entries {
        let projection = catalog.project(entry)?;
        let ModelCostFact::Known {
            amount_micros,
            reference,
        } = projection.cost
        else {
            summary.unknown_usage_ids.push(projection.provider_usage_id);
            continue;
        };
        let (tasks, sessions) = match projection.class {
            ModelCostClass::Execution => (
                &mut summary.execution_by_task,
                &mut summary.execution_by_session,
            ),
            ModelCostClass::Verification => (
                &mut summary.verification_by_task,
                &mut summary.verification_by_session,
            ),
        };
        add_bucket(
            tasks.entry(projection.execution_job_id.0).or_default(),
            &reference,
            amount_micros,
        )?;
        add_bucket(
            sessions.entry(projection.product_session_id.0).or_default(),
            &reference,
            amount_micros,
        )?;
    }
    summary.unknown_usage_ids.sort();
    Ok(summary)
}

fn add_bucket(
    buckets: &mut Vec<ModelCostBucket>,
    reference: &ModelPriceReference,
    amount_micros: u64,
) -> Result<(), ModelCostError> {
    if let Some(bucket) = buckets
        .iter_mut()
        .find(|bucket| bucket.reference == *reference)
    {
        bucket.amount_micros = bucket
            .amount_micros
            .checked_add(amount_micros)
            .ok_or(ModelCostError)?;
    } else {
        buckets.push(ModelCostBucket {
            reference: reference.clone(),
            amount_micros,
        });
        buckets.sort_by(|left, right| left.reference.cmp(&right.reference));
    }
    Ok(())
}

/// Explicit handling for usage whose price is not known.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UnknownCostPolicy {
    Stop,
    Continue,
}

/// Soft/hard cost limits under one exact billing semantic.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelBudgetPolicy {
    pub reference: ModelPriceReference,
    pub soft_limit_micros: u64,
    pub hard_limit_micros: u64,
    pub unknown_cost: UnknownCostPolicy,
}

/// Budget state returned at a safe execution boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelBudgetState {
    WithinBudget,
    SoftLimitReached,
    HardLimitReached,
    UnknownCost,
}

/// Budget control is not a task result and therefore has no failure variant.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelBudgetAction {
    Continue,
    StopAtSafePoint,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ModelBudgetDecision {
    pub state: ModelBudgetState,
    pub action: ModelBudgetAction,
    pub task_failed: bool,
}

impl ModelBudgetPolicy {
    /// Evaluates one execution or verification ledger at its next safe point.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed limits, references, or cost overflow.
    pub fn evaluate(
        &self,
        projections: &[ModelCostProjection],
        class: ModelCostClass,
    ) -> Result<ModelBudgetDecision, ModelCostError> {
        validate_reference(&self.reference)?;
        if self.soft_limit_micros > self.hard_limit_micros
            || self.hard_limit_micros == 0
            || self.hard_limit_micros > MAX_SAFE_INTEGER
        {
            return Err(ModelCostError);
        }
        let mut total = 0_u64;
        let mut unknown = false;
        for projection in projections
            .iter()
            .filter(|projection| projection.class == class)
        {
            match &projection.cost {
                ModelCostFact::Known {
                    amount_micros,
                    reference,
                } if reference == &self.reference => {
                    total = total.checked_add(*amount_micros).ok_or(ModelCostError)?;
                }
                ModelCostFact::Known { .. } | ModelCostFact::Unknown => unknown = true,
            }
        }
        let (state, action) = if unknown {
            let action = match self.unknown_cost {
                UnknownCostPolicy::Stop => ModelBudgetAction::StopAtSafePoint,
                UnknownCostPolicy::Continue => ModelBudgetAction::Continue,
            };
            (ModelBudgetState::UnknownCost, action)
        } else if total >= self.hard_limit_micros {
            (
                ModelBudgetState::HardLimitReached,
                ModelBudgetAction::StopAtSafePoint,
            )
        } else if total >= self.soft_limit_micros {
            (
                ModelBudgetState::SoftLimitReached,
                ModelBudgetAction::Continue,
            )
        } else {
            (ModelBudgetState::WithinBudget, ModelBudgetAction::Continue)
        };
        Ok(ModelBudgetDecision {
            state,
            action,
            task_failed: false,
        })
    }
}

/// Bounded validation or arithmetic failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ModelCostError;

impl fmt::Display for ModelCostError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("model price or cost facts are invalid")
    }
}

impl std::error::Error for ModelCostError {}

fn calculate_cost(
    usage: &crate::SettledModelUsage,
    price: &ModelPriceEntry,
) -> Result<u64, ModelCostError> {
    let ordinary_input = usage
        .input_tokens
        .checked_sub(usage.cached_input_tokens)
        .and_then(|value| value.checked_sub(usage.cache_write_input_tokens))
        .ok_or(ModelCostError)?;
    let ordinary_output = usage
        .output_tokens
        .checked_sub(usage.reasoning_output_tokens)
        .ok_or(ModelCostError)?;
    let rates = price.micros_per_million_tokens;
    let components = [
        (ordinary_input, rates.input),
        (usage.cached_input_tokens, rates.cached_input),
        (usage.cache_write_input_tokens, rates.cache_write_input),
        (ordinary_output, rates.output),
        (usage.reasoning_output_tokens, rates.reasoning_output),
    ];
    let numerator = components
        .into_iter()
        .try_fold(0_u128, |total, (tokens, rate)| {
            total
                .checked_add(u128::from(tokens) * u128::from(rate))
                .ok_or(ModelCostError)
        })?;
    let rounded = numerator
        .checked_add(TOKENS_PER_MILLION - 1)
        .ok_or(ModelCostError)?
        / TOKENS_PER_MILLION;
    u64::try_from(rounded)
        .ok()
        .filter(|value| *value <= MAX_SAFE_INTEGER)
        .ok_or(ModelCostError)
}

fn validate_reference(reference: &ModelPriceReference) -> Result<(), ModelCostError> {
    validate_token(&reference.source, 200)?;
    validate_token(&reference.version, 200)?;
    validate_token(&reference.currency, 16)?;
    validate_token(&reference.billing_semantics, 200)
}

fn validate_token(value: &str, max_chars: usize) -> Result<(), ModelCostError> {
    if value.trim().is_empty()
        || value.chars().count() > max_chars
        || value.chars().any(char::is_control)
    {
        Err(ModelCostError)
    } else {
        Ok(())
    }
}
