//! Logical-replica-safe remote history projection for the TUI Overview.
//!
//! The input is the independent `AllIncluded` unified history query, never a
//! direct sum of per-source SSH buckets. `history_query` has already resolved
//! logical replicas and selected one authority before this projection runs.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};

#[cfg(test)]
use crate::api_cost::API_PRICING_CATALOG_REVISION;
use crate::api_cost::current_api_pricing_catalog_revision;
use crate::domain::{
    ApiCostAmount, ApiEquivalentCost, ApiModelCost, Confidence, Provenance, TaskRecord, TaskStatus,
    ThreadWindowUsage, TokenUsage, TurnRecord, TurnStatus, TurnWindowUsage, WindowAnalysis,
    WindowUsage,
};
use crate::history::{
    HISTORY_PROJECT_BREAKDOWN_REVISION, HistoryData, LocalHalfHourBucket, LocalProjectUsageGroup,
    LocalUsageGroup, current_history_estimator_revision,
};
use crate::source_identity::NodeId;

const LOGICAL_THREAD_PREFIX: &str = "logical-thread:";

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct RemoteOverviewHistory {
    /// Remote-attributed project groups used for task and turn projection.
    buckets: Arc<Vec<LocalHalfHourBucket>>,
    /// The complete `AllIncluded` buckets used for the Models panel. Replacing
    /// the local model rows with this already-deduplicated projection avoids
    /// adding a logical replica once locally and once remotely.
    unified_buckets: Arc<Vec<LocalHalfHourBucket>>,
    warnings: Arc<Vec<String>>,
    /// At least one unified-history warning means that the all-source API
    /// total itself may be incomplete or duplicated. Project-label and tree
    /// warnings deliberately do not set this bit.
    api_history_warning: bool,
    /// Included SSH node IDs and presentation labels. IDs classify scoped
    /// `raw@node` identities; labels are used only for visible source names.
    remote_sources: BTreeMap<String, String>,
}

impl RemoteOverviewHistory {
    pub(crate) fn from_unified(
        history: &HistoryData,
        remote_sources: impl IntoIterator<Item = (NodeId, String)>,
        as_of: DateTime<Utc>,
    ) -> Self {
        let remote_sources = remote_sources
            .into_iter()
            .map(|(node, label)| (node.as_str().to_owned(), label))
            .collect::<BTreeMap<_, _>>();
        let since = as_of
            .checked_sub_signed(Duration::days(8))
            .unwrap_or(DateTime::<Utc>::MIN_UTC);
        let missing_project_breakdown = history.half_hour_buckets.iter().any(|bucket| {
            bucket.ends_at > since
                && bucket.sampled_at <= as_of
                && bucket.project_breakdown_revision != HISTORY_PROJECT_BREAKDOWN_REVISION
                && bucket.project_groups.iter().any(|group| {
                    classify_thread(&group.thread_id, &remote_sources).is_some()
                        && !group.token_usage.is_zero()
                })
        });
        let unified_buckets = history
            .half_hour_buckets
            .iter()
            .filter(|bucket| bucket.ends_at > since && bucket.sampled_at <= as_of)
            .cloned()
            .collect::<Vec<_>>();
        let buckets = unified_buckets
            .iter()
            .filter_map(|bucket| {
                let mut bucket = bucket.clone();
                bucket.groups.clear();
                bucket
                    .project_groups
                    .retain(|group| classify_thread(&group.thread_id, &remote_sources).is_some());
                (!bucket.project_groups.is_empty()).then_some(bucket)
            })
            .collect();
        let api_history_warning = history
            .warnings
            .iter()
            .any(|warning| remote_history_warning_affects_api_total(warning));
        let mut warnings = history
            .warnings
            .iter()
            .map(|warning| format!("remote history unified query: {warning}"))
            .collect::<Vec<_>>();
        if missing_project_breakdown {
            warnings.push(
                "remote history unified query: project breakdown unavailable for retained usage"
                    .to_owned(),
            );
        }
        warnings.sort();
        warnings.dedup();
        Self {
            buckets: Arc::new(buckets),
            unified_buckets: Arc::new(unified_buckets),
            warnings: Arc::new(warnings),
            api_history_warning,
            remote_sources,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct RemoteOverviewProjection {
    pub(crate) tasks: Vec<TaskRecord>,
    pub(crate) turns: Vec<TurnRecord>,
    pub(crate) windows: Vec<RemoteOverviewWindow>,
    pub(crate) warnings: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct RemoteOverviewWindow {
    pub(crate) duration_mins: i64,
    pub(crate) threads: Vec<ThreadWindowUsage>,
    pub(crate) turns: Vec<TurnWindowUsage>,
    /// Complete all-source model projection for this window. `None` means the
    /// window contains no remote-attributed usage and the collector's local
    /// analysis should remain untouched.
    pub(crate) models: Option<Vec<RemoteOverviewModelUsage>>,
    pub(crate) model_token_usage: TokenUsage,
    pub(crate) estimated_cost_units: u128,
    pub(crate) api_long_context_extra_cost_units: Option<u128>,
    pub(crate) api_equivalent_cost: Option<ApiEquivalentCost>,
    /// Canonical logical threads replace the collector's raw local row rather
    /// than being added to it. This is the key to replica-safe overlay.
    pub(crate) replaced_local_threads: BTreeSet<String>,
    pub(crate) partial_reasons: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct RemoteOverviewModelUsage {
    pub(crate) model: String,
    pub(crate) token_usage: TokenUsage,
    pub(crate) estimated_cost_units: u128,
    pub(crate) api_long_context_extra_cost_units: Option<u128>,
    pub(crate) api_equivalent_cost: ApiCostAmount,
    pub(crate) api_equivalent_cost_complete: bool,
    pub(crate) api_equivalent_cost_incomplete_tokens: u64,
    pub(crate) api_equivalent_cost_incomplete_samples: u64,
    pub(crate) call_count: u64,
}

#[derive(Clone, Debug, Default)]
struct UsageAggregate {
    tokens: TokenUsage,
    api_cost: ApiCostAmount,
}

impl UsageAggregate {
    fn add_group(&mut self, group: &LocalProjectUsageGroup, pricing_catalog_current: bool) {
        self.tokens.add_assign(group.token_usage);
        self.api_cost.add_assign(if pricing_catalog_current {
            group.api_equivalent_cost
        } else {
            unpriced_api_cost(group)
        });
    }

    fn window_usage(&self) -> WindowUsage {
        WindowUsage {
            token_usage: self.tokens,
            local_token_share_percent: 0.0,
            // The persisted Snapshot does not expose the collector's absolute
            // local cost-unit denominator. Keep remote quota share unknown
            // rather than mixing incompatible denominators.
            estimated_quota_percent: 0.0,
            quota_confidence: Confidence::Unknown,
            api_equivalent_cost: self.api_cost,
        }
    }
}

#[derive(Clone, Debug)]
struct ModelAggregate {
    tokens: TokenUsage,
    estimated_cost_units: u128,
    api_long_context_extra_cost_units: Option<u128>,
    api_cost: ApiCostAmount,
    api_cost_complete: bool,
    api_cost_incomplete_tokens: u64,
    api_cost_incomplete_samples: u64,
    call_count: u64,
}

impl Default for ModelAggregate {
    fn default() -> Self {
        Self {
            tokens: TokenUsage::default(),
            estimated_cost_units: 0,
            api_long_context_extra_cost_units: Some(0),
            api_cost: ApiCostAmount::default(),
            api_cost_complete: true,
            api_cost_incomplete_tokens: 0,
            api_cost_incomplete_samples: 0,
            call_count: 0,
        }
    }
}

impl ModelAggregate {
    fn add_group(
        &mut self,
        group: &LocalUsageGroup,
        estimator_current: bool,
        pricing_catalog_current: bool,
    ) {
        self.tokens.add_assign(group.token_usage);
        if estimator_current {
            self.estimated_cost_units = self
                .estimated_cost_units
                .saturating_add(group.estimated_cost_units);
            self.api_long_context_extra_cost_units = add_optional_units(
                self.api_long_context_extra_cost_units,
                group.api_long_context_extra_cost_units,
            );
        }
        let mut api_cost = group.api_equivalent_cost;
        let complete = pricing_catalog_current && group.api_equivalent_cost_complete;
        if !complete {
            api_cost.observed_samples = api_cost.observed_samples.max(group.call_count);
            api_cost.observed_tokens = api_cost.observed_tokens.max(group.token_usage.total_tokens);
            if !pricing_catalog_current {
                api_cost.minimum_pico_usd = Default::default();
                api_cost.maximum_pico_usd = Default::default();
                api_cost.priced_samples = 0;
                api_cost.priced_tokens = 0;
            }
        }
        self.api_cost.add_assign(api_cost);
        self.api_cost_complete &= complete;
        if !complete {
            self.api_cost_incomplete_tokens = self
                .api_cost_incomplete_tokens
                .saturating_add(group.token_usage.total_tokens);
            self.api_cost_incomplete_samples = self
                .api_cost_incomplete_samples
                .saturating_add(group.call_count);
        }
        self.call_count = self.call_count.saturating_add(group.call_count);
    }

    fn add_opaque_residual(
        &mut self,
        tokens: TokenUsage,
        estimated_cost_units: u128,
        api_long_context_extra_cost_units: Option<u128>,
        call_count: u64,
    ) {
        self.tokens.add_assign(tokens);
        self.estimated_cost_units = self
            .estimated_cost_units
            .saturating_add(estimated_cost_units);
        self.api_long_context_extra_cost_units = add_optional_units(
            self.api_long_context_extra_cost_units,
            api_long_context_extra_cost_units,
        );
        self.api_cost.observed_samples = self.api_cost.observed_samples.saturating_add(call_count);
        self.api_cost.observed_tokens = self
            .api_cost
            .observed_tokens
            .saturating_add(tokens.total_tokens);
        self.api_cost_complete = false;
        self.api_cost_incomplete_tokens = self
            .api_cost_incomplete_tokens
            .saturating_add(tokens.total_tokens);
        self.api_cost_incomplete_samples =
            self.api_cost_incomplete_samples.saturating_add(call_count);
        self.call_count = self.call_count.saturating_add(call_count);
    }

    fn remote_usage(self, model: String) -> RemoteOverviewModelUsage {
        RemoteOverviewModelUsage {
            model,
            token_usage: self.tokens,
            estimated_cost_units: self.estimated_cost_units,
            api_long_context_extra_cost_units: self.api_long_context_extra_cost_units,
            api_equivalent_cost: self.api_cost,
            api_equivalent_cost_complete: self.api_cost_complete,
            api_equivalent_cost_incomplete_tokens: self.api_cost_incomplete_tokens,
            api_equivalent_cost_incomplete_samples: self.api_cost_incomplete_samples,
            call_count: self.call_count,
        }
    }
}

#[derive(Clone, Debug)]
struct UnifiedModelAggregate {
    tokens: TokenUsage,
    estimated_cost_units: u128,
    api_long_context_extra_cost_units: Option<u128>,
    long_context_usage_unknown: bool,
    api_cost: ApiCostAmount,
    call_count: u64,
    models: BTreeMap<String, ModelAggregate>,
    partial_reasons: BTreeSet<String>,
    api_partial_reasons: BTreeSet<String>,
}

impl Default for UnifiedModelAggregate {
    fn default() -> Self {
        Self {
            tokens: TokenUsage::default(),
            estimated_cost_units: 0,
            api_long_context_extra_cost_units: Some(0),
            long_context_usage_unknown: false,
            api_cost: ApiCostAmount::default(),
            call_count: 0,
            models: BTreeMap::new(),
            partial_reasons: BTreeSet::new(),
            api_partial_reasons: BTreeSet::new(),
        }
    }
}

impl UnifiedModelAggregate {
    fn add_bucket(&mut self, bucket: &LocalHalfHourBucket) {
        self.tokens.add_assign(bucket.token_usage);
        let estimator_current = bucket.estimator_revision == current_history_estimator_revision();
        if estimator_current {
            self.estimated_cost_units = self
                .estimated_cost_units
                .saturating_add(bucket.estimated_cost_units);
            self.api_long_context_extra_cost_units = add_optional_units(
                self.api_long_context_extra_cost_units,
                bucket.api_long_context_extra_cost_units,
            );
            self.long_context_usage_unknown |= bucket.long_context_usage_unknown;
        } else if !bucket.token_usage.is_zero()
            || bucket.estimated_cost_units > 0
            || bucket
                .api_long_context_extra_cost_units
                .is_some_and(|units| units > 0)
            || bucket.call_count > 0
        {
            self.partial_reasons
                .insert("remote_estimator_revision_mismatch".to_owned());
        }
        self.call_count = self.call_count.saturating_add(bucket.call_count);
        let pricing_catalog_current =
            bucket.api_pricing_catalog_revision == current_api_pricing_catalog_revision();

        let mut bucket_api_cost = ApiCostAmount::default();
        let mut project_tokens = TokenUsage::default();
        for group in &bucket.project_groups {
            project_tokens.add_assign(group.token_usage);
            bucket_api_cost.add_assign(if pricing_catalog_current {
                group.api_equivalent_cost
            } else {
                unpriced_api_cost(group)
            });
        }
        let project_calls = bucket
            .project_groups
            .iter()
            .map(|group| group.call_count)
            .fold(0_u64, u64::saturating_add);
        let project_breakdown_complete = project_tokens == bucket.token_usage
            && project_calls >= bucket.call_count
            && bucket_api_cost.observed_samples >= bucket.call_count
            && bucket_api_cost.observed_tokens >= bucket.token_usage.total_tokens;
        if !project_breakdown_complete || !pricing_catalog_current {
            bucket_api_cost.observed_samples =
                bucket_api_cost.observed_samples.max(bucket.call_count);
            bucket_api_cost.observed_tokens = bucket_api_cost
                .observed_tokens
                .max(bucket.token_usage.total_tokens);
            self.partial_reasons
                .insert("remote_api_total_partial".to_owned());
            self.api_partial_reasons
                .insert("remote_api_total_partial".to_owned());
        }
        self.api_cost.add_assign(bucket_api_cost);

        for group in &bucket.groups {
            let model = normalized_model_name(group.model.as_deref()).to_owned();
            self.models.entry(model).or_default().add_group(
                group,
                estimator_current,
                pricing_catalog_current,
            );
            if group.used_model_fallback {
                self.partial_reasons
                    .insert("unpriced_model_rate_fallback".to_owned());
            }
            if group.used_token_breakdown_fallback {
                self.partial_reasons
                    .insert("token_breakdown_missing".to_owned());
            }
            if group.used_long_context_detection_fallback {
                self.partial_reasons
                    .insert("long_context_usage_unknown".to_owned());
            }
            if !group.api_equivalent_cost_complete {
                self.partial_reasons
                    .insert("remote_model_api_cost_partial".to_owned());
            }
        }
        if bucket
            .partial_reasons
            .iter()
            .any(|reason| reason == "duplicate_session_model_breakdown_partial")
        {
            self.partial_reasons
                .insert("remote_model_breakdown_partial".to_owned());
        }
        if !bucket.partial_reasons.is_empty() {
            self.partial_reasons
                .insert("remote_history_bucket_partial".to_owned());
        }
        if bucket
            .partial_reasons
            .iter()
            .any(|reason| remote_bucket_reason_affects_api_total(reason))
        {
            self.api_partial_reasons
                .insert("remote_api_history_coverage_partial".to_owned());
        }
    }

    fn finish(mut self) -> FinishedUnifiedModels {
        let mut grouped_tokens = TokenUsage::default();
        let mut grouped_cost_units = 0_u128;
        let mut grouped_long_extra = Some(0_u128);
        let mut grouped_calls = 0_u64;
        for model in self.models.values() {
            grouped_tokens.add_assign(model.tokens);
            grouped_cost_units = grouped_cost_units.saturating_add(model.estimated_cost_units);
            grouped_long_extra =
                add_optional_units(grouped_long_extra, model.api_long_context_extra_cost_units);
            grouped_calls = grouped_calls.saturating_add(model.call_count);
        }

        let token_residual = self.tokens.delta_from(grouped_tokens);
        let cost_residual = self.estimated_cost_units.checked_sub(grouped_cost_units);
        let call_residual = self.call_count.checked_sub(grouped_calls);
        let long_residual = match (self.api_long_context_extra_cost_units, grouped_long_extra) {
            (Some(total), Some(grouped)) => total.checked_sub(grouped),
            (None, _) | (_, None) => None,
        };
        let breakdown_consistent = token_residual.is_some()
            && cost_residual.is_some()
            && call_residual.is_some()
            && (self.api_long_context_extra_cost_units.is_none() || long_residual.is_some());

        if breakdown_consistent {
            let token_residual = token_residual.unwrap_or_default();
            let cost_residual = cost_residual.unwrap_or_default();
            let call_residual = call_residual.unwrap_or_default();
            if !token_residual.is_zero() || cost_residual > 0 || call_residual > 0 {
                self.models
                    .entry("unknown".to_owned())
                    .or_default()
                    .add_opaque_residual(
                        token_residual,
                        cost_residual,
                        long_residual,
                        call_residual,
                    );
                self.partial_reasons
                    .insert("remote_model_breakdown_partial".to_owned());
                self.partial_reasons
                    .insert("remote_model_api_cost_partial".to_owned());
            }
        } else {
            let mut unknown = ModelAggregate::default();
            unknown.add_opaque_residual(
                self.tokens,
                self.estimated_cost_units,
                self.api_long_context_extra_cost_units,
                self.call_count,
            );
            self.models.clear();
            self.models.insert("unknown".to_owned(), unknown);
            self.partial_reasons
                .insert("remote_model_breakdown_inconsistent".to_owned());
            self.partial_reasons
                .insert("remote_model_api_cost_partial".to_owned());
        }

        if self.long_context_usage_unknown || self.api_long_context_extra_cost_units.is_none() {
            self.partial_reasons
                .insert("long_context_usage_unknown".to_owned());
            self.api_long_context_extra_cost_units = None;
        }
        let models = self
            .models
            .into_iter()
            .map(|(model, usage)| usage.remote_usage(model))
            .collect::<Vec<_>>();
        let model_breakdown = models
            .iter()
            .map(|model| ApiModelCost {
                model: model.model.clone(),
                amount: model.api_equivalent_cost,
            })
            .collect::<Vec<_>>();
        let mut model_api_cost = ApiCostAmount::default();
        for model in &model_breakdown {
            model_api_cost.add_assign(model.amount);
        }
        if model_api_cost != self.api_cost {
            // API-only calls (for example Spark) are deliberately excluded
            // from quota-model groups. Older persisted buckets therefore
            // cannot always name the model behind every dollar in the exact
            // aggregate. Keep the total intact while making that narrower
            // breakdown limitation explicit.
            self.partial_reasons
                .insert("remote_api_model_breakdown_partial".to_owned());
        }
        let partial_reasons = self.partial_reasons.into_iter().collect::<Vec<_>>();
        FinishedUnifiedModels {
            models,
            token_usage: self.tokens,
            estimated_cost_units: self.estimated_cost_units,
            api_long_context_extra_cost_units: self.api_long_context_extra_cost_units,
            api_equivalent_cost: ApiEquivalentCost {
                amount: self.api_cost,
                partial_reasons: self.api_partial_reasons.into_iter().collect(),
                model_breakdown,
            },
            partial_reasons,
        }
    }
}

struct FinishedUnifiedModels {
    models: Vec<RemoteOverviewModelUsage>,
    token_usage: TokenUsage,
    estimated_cost_units: u128,
    api_long_context_extra_cost_units: Option<u128>,
    api_equivalent_cost: ApiEquivalentCost,
    partial_reasons: Vec<String>,
}

fn add_optional_units(left: Option<u128>, right: Option<u128>) -> Option<u128> {
    Some(left?.saturating_add(right?))
}

fn normalized_model_name(model: Option<&str>) -> &str {
    model
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .unwrap_or("unknown")
}

fn unpriced_api_cost(group: &LocalProjectUsageGroup) -> ApiCostAmount {
    ApiCostAmount {
        observed_samples: group
            .api_equivalent_cost
            .observed_samples
            .max(group.call_count),
        observed_tokens: group
            .api_equivalent_cost
            .observed_tokens
            .max(group.token_usage.total_tokens),
        ..ApiCostAmount::default()
    }
}

#[derive(Clone, Debug, Default)]
struct TaskMetadata {
    thread_id: String,
    parent_thread_id: Option<String>,
    parent_thread_conflict: bool,
    session_thread_id: Option<String>,
    session_thread_conflict: bool,
    source: Option<String>,
    title: Option<String>,
    project_label: Option<String>,
    created_at: Option<DateTime<Utc>>,
    updated_at: Option<DateTime<Utc>>,
    turns: BTreeSet<String>,
    usage: UsageAggregate,
}

#[derive(Clone, Debug, Default)]
struct TurnMetadata {
    thread_id: String,
    turn_id: String,
    message_preview: Option<String>,
    started_at: Option<DateTime<Utc>>,
    usage: UsageAggregate,
}

#[derive(Clone, Debug)]
struct ClassifiedThread {
    canonical: String,
    source: Option<String>,
    logical: bool,
}

pub(crate) fn project_remote_overview_history(
    history: &RemoteOverviewHistory,
    analyses: &[WindowAnalysis],
    as_of: DateTime<Utc>,
) -> RemoteOverviewProjection {
    let windows = analyses
        .iter()
        .filter_map(|analysis| {
            analysis
                .attribution
                .window
                .as_ref()
                .map(|window| (analysis.duration_mins, window.starts_at, window.ends_at))
        })
        .collect::<Vec<_>>();
    let local_window_tokens = analyses
        .iter()
        .map(|analysis| {
            (
                analysis.duration_mins,
                analysis.attribution.local_token_usage,
            )
        })
        .collect::<BTreeMap<_, _>>();
    let mut tasks = BTreeMap::<String, TaskMetadata>::new();
    let mut turns = BTreeMap::<(String, String), TurnMetadata>::new();
    let mut window_threads = windows
        .iter()
        .map(|(duration, _, _)| (*duration, BTreeMap::<String, UsageAggregate>::new()))
        .collect::<BTreeMap<_, _>>();
    let mut window_turns = windows
        .iter()
        .map(|(duration, _, _)| {
            (
                *duration,
                BTreeMap::<(String, String), UsageAggregate>::new(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let mut window_models = windows
        .iter()
        .map(|(duration, _, _)| (*duration, UnifiedModelAggregate::default()))
        .collect::<BTreeMap<_, _>>();
    let mut replaced_local_threads = BTreeMap::<i64, BTreeSet<String>>::new();
    let mut window_reasons = BTreeMap::<i64, BTreeSet<String>>::new();
    for (duration, starts_at, ends_at) in &windows {
        if history.unified_buckets.iter().any(|bucket| {
            bucket.sampled_at <= as_of
                && ((bucket.starts_at < *starts_at && bucket.ends_at > *starts_at)
                    || (bucket.starts_at < *ends_at && bucket.ends_at > *ends_at))
        }) {
            window_reasons
                .entry(*duration)
                .or_default()
                .insert("remote_window_boundary_lower_bound".to_owned());
        }
        if !history.warnings.is_empty() {
            window_reasons
                .entry(*duration)
                .or_default()
                .insert("remote_unified_history_warning".to_owned());
        }
        if history.api_history_warning {
            window_reasons
                .entry(*duration)
                .or_default()
                .insert("remote_api_history_warning".to_owned());
        }
    }

    // Models are replaced from the complete AllIncluded query, not appended
    // from physical remote sources.  That query has already selected one
    // logical-replica authority, so a local/remote copy can contribute only
    // once to this aggregate.
    for bucket in history.unified_buckets.iter() {
        for duration in windows
            .iter()
            .filter(|(_, starts_at, ends_at)| {
                bucket.starts_at >= *starts_at
                    && bucket.starts_at < *ends_at
                    && (bucket.ends_at <= *ends_at || bucket.sampled_at <= *ends_at)
                    && bucket.sampled_at <= as_of
            })
            .map(|(duration, _, _)| *duration)
        {
            window_models
                .entry(duration)
                .or_default()
                .add_bucket(bucket);
        }
    }

    for bucket in history.buckets.iter() {
        let selected_windows = windows
            .iter()
            .filter(|(_, starts_at, ends_at)| {
                bucket.starts_at >= *starts_at
                    && bucket.starts_at < *ends_at
                    && (bucket.ends_at <= *ends_at || bucket.sampled_at <= *ends_at)
                    && bucket.sampled_at <= as_of
            })
            .map(|(duration, _, _)| *duration)
            .collect::<Vec<_>>();
        if selected_windows.is_empty() {
            continue;
        }
        for group in &bucket.project_groups {
            let Some(classified) = classify_thread(&group.thread_id, &history.remote_sources)
            else {
                continue;
            };
            let parent = group.parent_thread_id.as_deref().and_then(|value| {
                canonical_reference(value, &history.remote_sources, classified.source.as_deref())
            });
            let session = group.session_thread_id.as_deref().and_then(|value| {
                canonical_reference(value, &history.remote_sources, classified.source.as_deref())
            });
            let turn_id = group
                .turn_id
                .as_deref()
                .map(strip_scoped_value)
                .map(str::to_owned);
            let delegated = parent.is_some();
            {
                let task = tasks
                    .entry(classified.canonical.clone())
                    .or_insert_with(|| TaskMetadata {
                        thread_id: classified.canonical.clone(),
                        source: classified.source.clone(),
                        ..TaskMetadata::default()
                    });
                merge_task_metadata(task, group, bucket, parent.clone(), session.clone());
                task.usage.add_group(
                    group,
                    bucket.api_pricing_catalog_revision == current_api_pricing_catalog_revision(),
                );
                if let Some(turn_id) = turn_id.as_ref() {
                    task.turns.insert(turn_id.clone());
                }
            }
            ensure_placeholder(
                &mut tasks,
                parent.as_deref(),
                session.as_deref(),
                classified.source.as_deref(),
            );
            ensure_placeholder(
                &mut tasks,
                session.as_deref(),
                None,
                classified.source.as_deref(),
            );

            if let Some(turn_id) = turn_id.as_ref() {
                let turn = turns
                    .entry((classified.canonical.clone(), turn_id.clone()))
                    .or_insert_with(|| TurnMetadata {
                        thread_id: classified.canonical.clone(),
                        turn_id: turn_id.clone(),
                        ..TurnMetadata::default()
                    });
                merge_turn_metadata(turn, group, delegated);
                turn.usage.add_group(
                    group,
                    bucket.api_pricing_catalog_revision == current_api_pricing_catalog_revision(),
                );
            }

            for duration in &selected_windows {
                window_threads
                    .entry(*duration)
                    .or_default()
                    .entry(classified.canonical.clone())
                    .or_default()
                    .add_group(
                        group,
                        bucket.api_pricing_catalog_revision
                            == current_api_pricing_catalog_revision(),
                    );
                if classified.logical {
                    replaced_local_threads
                        .entry(*duration)
                        .or_default()
                        .insert(classified.canonical.clone());
                }
                if let Some(turn_id) = turn_id.as_ref() {
                    window_turns
                        .entry(*duration)
                        .or_default()
                        .entry((classified.canonical.clone(), turn_id.clone()))
                        .or_default()
                        .add_group(
                            group,
                            bucket.api_pricing_catalog_revision
                                == current_api_pricing_catalog_revision(),
                        );
                }
            }
        }
        for duration in selected_windows {
            if bucket.project_breakdown_revision != HISTORY_PROJECT_BREAKDOWN_REVISION {
                window_reasons
                    .entry(duration)
                    .or_default()
                    .insert("remote_project_breakdown_revision_mismatch".to_string());
            }
            if bucket.api_pricing_catalog_revision != current_api_pricing_catalog_revision()
                && bucket.project_groups.iter().any(|group| {
                    !group.token_usage.is_zero()
                        || group.api_equivalent_cost.observed_samples > 0
                        || group.api_equivalent_cost.observed_tokens > 0
                })
            {
                window_reasons
                    .entry(duration)
                    .or_default()
                    .insert("remote_api_pricing_catalog_revision_mismatch".to_string());
            }
            if !bucket.partial_reasons.is_empty() {
                window_reasons
                    .entry(duration)
                    .or_default()
                    .insert("remote_history_bucket_partial".to_string());
            }
        }
    }

    if tasks
        .values()
        .any(|task| task.parent_thread_conflict || task.session_thread_conflict)
    {
        for (duration, _, _) in &windows {
            window_reasons
                .entry(*duration)
                .or_default()
                .insert("remote_history_lineage_conflict".to_owned());
        }
    }
    close_ancestors(&mut tasks);
    let mut projection = RemoteOverviewProjection {
        tasks: tasks.into_values().map(task_record).collect(),
        turns: turns.into_values().map(turn_record).collect(),
        windows: Vec::new(),
        warnings: history.warnings.as_ref().clone(),
    };
    for (duration_mins, threads) in window_threads {
        let unified_models = window_models.remove(&duration_mins).unwrap_or_default();
        let has_attributed_remote_usage = threads.values().any(|usage| {
            !usage.tokens.is_zero()
                || usage.api_cost.observed_samples > 0
                || usage.api_cost.observed_tokens > 0
        });
        // Older or partially reconstructed remote buckets can retain a full
        // model/token total while lacking thread/project groups. If the
        // replica-safe all-source total strictly exceeds the live local
        // window, that residual is remote evidence even though it cannot be
        // placed in the task tree. Show it in Models instead of silently
        // falling back to local-only rows.
        let has_opaque_remote_usage = !history.remote_sources.is_empty()
            && local_window_tokens
                .get(&duration_mins)
                .is_some_and(|local| unified_models.tokens.total_tokens > local.total_tokens);
        let has_remote_usage = has_attributed_remote_usage || has_opaque_remote_usage;
        // Merely configuring a remote source must not replace the collector's
        // fresher local Models view. Switch to the all-source history
        // projection only after this window contains attributable remote
        // usage.
        let finished_models = has_remote_usage.then(|| unified_models.finish());
        let mut partial_reasons = window_reasons.remove(&duration_mins).unwrap_or_default();
        if has_opaque_remote_usage && !has_attributed_remote_usage {
            partial_reasons.insert("remote_project_breakdown_unavailable".to_owned());
        }
        if let Some(finished) = &finished_models {
            partial_reasons.extend(finished.partial_reasons.iter().cloned());
        }
        let mut api_equivalent_cost = finished_models
            .as_ref()
            .map(|finished| finished.api_equivalent_cost.clone());
        if let Some(api_cost) = api_equivalent_cost.as_mut() {
            api_cost.partial_reasons.extend(
                partial_reasons
                    .iter()
                    .filter(|reason| remote_window_reason_affects_api_total(reason))
                    .cloned(),
            );
            api_cost.partial_reasons.sort();
            api_cost.partial_reasons.dedup();
        }
        projection.windows.push(RemoteOverviewWindow {
            duration_mins,
            threads: threads
                .into_iter()
                .map(|(thread_id, usage)| ThreadWindowUsage {
                    thread_id,
                    usage: usage.window_usage(),
                })
                .collect(),
            turns: window_turns
                .remove(&duration_mins)
                .unwrap_or_default()
                .into_iter()
                .map(|((thread_id, turn_id), usage)| TurnWindowUsage {
                    thread_id,
                    turn_id,
                    usage: usage.window_usage(),
                })
                .collect(),
            models: finished_models
                .as_ref()
                .map(|finished| finished.models.clone()),
            model_token_usage: finished_models
                .as_ref()
                .map_or_else(TokenUsage::default, |finished| finished.token_usage),
            estimated_cost_units: finished_models
                .as_ref()
                .map_or(0, |finished| finished.estimated_cost_units),
            api_long_context_extra_cost_units: finished_models
                .as_ref()
                .and_then(|finished| finished.api_long_context_extra_cost_units),
            api_equivalent_cost,
            replaced_local_threads: replaced_local_threads
                .remove(&duration_mins)
                .unwrap_or_default(),
            partial_reasons: partial_reasons.into_iter().collect(),
        });
    }
    projection
        .tasks
        .sort_by(|left, right| left.thread_id.cmp(&right.thread_id));
    projection.turns.sort_by(|left, right| {
        left.thread_id
            .cmp(&right.thread_id)
            .then_with(|| left.turn_id.cmp(&right.turn_id))
    });
    projection
        .windows
        .sort_by_key(|window| window.duration_mins);
    projection
}

fn remote_window_reason_affects_api_total(reason: &str) -> bool {
    matches!(
        reason,
        "remote_window_boundary_lower_bound"
            | "remote_api_history_warning"
            | "remote_api_total_partial"
            | "remote_api_pricing_catalog_revision_mismatch"
    )
}

fn remote_bucket_reason_affects_api_total(reason: &str) -> bool {
    matches!(
        reason,
        "rollout_local_coverage_unverified"
            | "coverage_starts_within_local_bucket"
            | "rollout_scan_incomplete"
            | "rollout_scan_truncated"
            | "rollout_unreadable"
            | "rollout_lines_skipped"
            | "ambiguous_token_reset"
    )
}

fn remote_history_warning_affects_api_total(warning: &str) -> bool {
    // These warnings only affect project naming, tree placement, or an exact
    // source selector that AllIncluded intentionally omitted. Every other
    // warning is conservatively treated as possible source/replica loss.
    ![
        "project_mapping_partial",
        "project_mapping_unavailable",
        "project_mapping_registration_failed",
        "replica_project_conflict",
        "source_selection_excluded_from_aggregates",
    ]
    .iter()
    .any(|prefix| warning == *prefix || warning.starts_with(&format!("{prefix}:")))
}

fn classify_thread(
    value: &str,
    remote_sources: &BTreeMap<String, String>,
) -> Option<ClassifiedThread> {
    if let Some(raw) = value.strip_prefix(LOGICAL_THREAD_PREFIX) {
        return Some(ClassifiedThread {
            canonical: raw.to_owned(),
            source: Some("remote:replica".to_owned()),
            logical: true,
        });
    }
    let (raw, node) = split_scoped_value(value)?;
    let label = remote_sources.get(node)?;
    Some(ClassifiedThread {
        canonical: format!("remote:{node}:{raw}"),
        source: Some(format!("remote:{label}")),
        logical: false,
    })
}

fn canonical_reference(
    value: &str,
    remote_sources: &BTreeMap<String, String>,
    fallback_source: Option<&str>,
) -> Option<String> {
    if let Some(raw) = value.strip_prefix(LOGICAL_THREAD_PREFIX) {
        return Some(raw.to_owned());
    }
    if let Some((raw, node)) = split_scoped_value(value) {
        if remote_sources.contains_key(node) {
            return Some(format!("remote:{node}:{raw}"));
        }
        return Some(raw.to_owned());
    }
    fallback_source
        .and_then(|source| source.strip_prefix("remote:"))
        .map_or_else(|| Some(value.to_owned()), |_| Some(value.to_owned()))
}

fn split_scoped_value(value: &str) -> Option<(&str, &str)> {
    let (raw, node) = value.rsplit_once('@')?;
    node.starts_with("node-").then_some((raw, node))
}

fn strip_scoped_value(value: &str) -> &str {
    split_scoped_value(value).map_or(value, |(raw, _)| raw)
}

fn merge_task_metadata(
    task: &mut TaskMetadata,
    group: &LocalProjectUsageGroup,
    bucket: &LocalHalfHourBucket,
    parent: Option<String>,
    session: Option<String>,
) {
    merge_lineage_reference(
        &mut task.parent_thread_id,
        &mut task.parent_thread_conflict,
        parent,
    );
    merge_lineage_reference(
        &mut task.session_thread_id,
        &mut task.session_thread_conflict,
        session,
    );
    if task.title.is_none() {
        task.title = group.title.clone();
    }
    if task.project_label.is_none() {
        task.project_label = group.project_label.clone();
    }
    task.created_at = min_timestamp(task.created_at, group.turn_started_at);
    task.updated_at = max_timestamp(task.updated_at, Some(bucket.sampled_at));
}

fn merge_lineage_reference(
    current: &mut Option<String>,
    conflicted: &mut bool,
    incoming: Option<String>,
) {
    if *conflicted {
        return;
    }
    let Some(incoming) = incoming else { return };
    match current {
        Some(existing) if existing != &incoming => {
            *current = None;
            *conflicted = true;
        }
        Some(_) => {}
        None => *current = Some(incoming),
    }
}

fn merge_turn_metadata(turn: &mut TurnMetadata, group: &LocalProjectUsageGroup, delegated: bool) {
    // Delegated groups inherit the root user prompt. It describes attribution,
    // not the subagent's emitting turn, so neither its text nor timestamp can
    // be assigned to that emitting turn.
    if !delegated {
        if turn.message_preview.is_none() {
            turn.message_preview = group.message_preview.clone();
        }
        turn.started_at = min_timestamp(turn.started_at, group.turn_started_at);
    }
}

fn ensure_placeholder(
    tasks: &mut BTreeMap<String, TaskMetadata>,
    id: Option<&str>,
    session: Option<&str>,
    source: Option<&str>,
) {
    let Some(id) = id else { return };
    let task = tasks.entry(id.to_owned()).or_insert_with(|| TaskMetadata {
        thread_id: id.to_owned(),
        source: source.map(str::to_owned),
        ..TaskMetadata::default()
    });
    if task.session_thread_id.is_none() && !task.session_thread_conflict {
        task.session_thread_id = session.map(str::to_owned);
    }
}

fn close_ancestors(tasks: &mut BTreeMap<String, TaskMetadata>) {
    let ids = tasks.keys().cloned().collect::<Vec<_>>();
    for id in ids {
        let Some(task) = tasks.get(&id) else { continue };
        let parent = task.parent_thread_id.clone();
        let session = task.session_thread_id.clone();
        let parent_conflicted = task.parent_thread_conflict;
        if let Some(session) = session.as_ref() {
            ensure_placeholder(tasks, Some(session), None, None);
            if id != *session
                && parent.is_none()
                && !parent_conflicted
                && let Some(task) = tasks.get_mut(&id)
            {
                task.parent_thread_id = Some(session.clone());
            }
            if let Some(parent) = parent
                && parent != *session
                && let Some(parent_task) = tasks.get_mut(&parent)
                && parent_task.parent_thread_id.is_none()
                && !parent_task.parent_thread_conflict
            {
                parent_task.parent_thread_id = Some(session.clone());
            }
        }
    }
}

fn task_record(task: TaskMetadata) -> TaskRecord {
    let short = short_thread_id(&task.thread_id).to_owned();
    TaskRecord {
        thread_id: task.thread_id,
        parent_thread_id: task.parent_thread_id,
        archived: false,
        title: task.title.unwrap_or_else(|| format!("Remote task {short}")),
        cwd: task.project_label.map(PathBuf::from),
        source: task.source,
        created_at: task.created_at,
        updated_at: task.updated_at,
        status: TaskStatus::Unknown,
        status_provenance: Provenance::Unknown,
        status_confidence: Confidence::Unknown,
        token_usage: task.usage.tokens,
        turn_count: task.turns.len(),
        window_token_usage: TokenUsage::default(),
        local_token_share_percent: 0.0,
        estimated_quota_percent: 0.0,
        quota_confidence: Confidence::Unknown,
        api_equivalent_cost: None,
    }
}

fn turn_record(turn: TurnMetadata) -> TurnRecord {
    TurnRecord {
        thread_id: turn.thread_id,
        turn_id: turn.turn_id,
        model: None,
        reasoning_effort: None,
        service_tier: None,
        message_preview: turn.message_preview,
        started_at: turn.started_at,
        completed_at: None,
        duration_ms: None,
        status: TurnStatus::Unknown,
        token_usage: turn.usage.tokens,
        window_token_usage: TokenUsage::default(),
        local_token_share_percent: 0.0,
        estimated_quota_percent: 0.0,
        quota_confidence: Confidence::Unknown,
        api_equivalent_cost: None,
    }
}

fn min_timestamp(
    left: Option<DateTime<Utc>>,
    right: Option<DateTime<Utc>>,
) -> Option<DateTime<Utc>> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.min(right)),
        (left, right) => left.or(right),
    }
}

fn max_timestamp(
    left: Option<DateTime<Utc>>,
    right: Option<DateTime<Utc>>,
) -> Option<DateTime<Utc>> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.max(right)),
        (left, right) => left.or(right),
    }
}

fn short_thread_id(thread_id: &str) -> &str {
    let end = thread_id
        .char_indices()
        .nth(8)
        .map_or(thread_id.len(), |(index, _)| index);
    &thread_id[..end]
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, TimeZone};

    use super::*;
    use crate::domain::{AttributionSummary, WindowDescriptor};
    use crate::history::{HISTORY_PROJECT_BREAKDOWN_REVISION, LocalHalfHourBucket};

    fn at(hour: u32, minute: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 31, hour, minute, 0).unwrap()
    }

    fn usage(tokens: u64) -> TokenUsage {
        TokenUsage {
            input_tokens: tokens,
            total_tokens: tokens,
            ..TokenUsage::default()
        }
    }

    fn group(
        thread: &str,
        turn: &str,
        parent: Option<&str>,
        tokens: u64,
    ) -> LocalProjectUsageGroup {
        LocalProjectUsageGroup {
            thread_id: thread.to_owned(),
            turn_id: Some(turn.to_owned()),
            parent_thread_id: parent.map(str::to_owned),
            session_thread_id: Some("root@node-0123456789abcdef0123456789abcdef".to_owned()),
            message_preview: Some(format!("root prompt {turn}")),
            project_label: Some("project".to_owned()),
            title: (thread.starts_with("root@")).then(|| "Root session".to_owned()),
            token_usage: usage(tokens),
            estimated_cost_units: u128::from(tokens),
            api_equivalent_cost: ApiCostAmount {
                observed_samples: 1,
                priced_samples: 1,
                observed_tokens: tokens,
                priced_tokens: tokens,
                ..ApiCostAmount::default()
            },
            call_count: 1,
            ..LocalProjectUsageGroup::default()
        }
    }

    fn bucket(start: DateTime<Utc>, groups: Vec<LocalProjectUsageGroup>) -> LocalHalfHourBucket {
        let mut token_usage = TokenUsage::default();
        for group in &groups {
            token_usage.add_assign(group.token_usage);
        }
        LocalHalfHourBucket {
            starts_at: start,
            ends_at: start + Duration::minutes(15),
            sampled_at: start + Duration::minutes(15),
            token_usage,
            estimated_cost_units: u128::from(token_usage.total_tokens),
            api_long_context_extra_cost_units: Some(0),
            long_context_usage_unknown: false,
            estimator_revision: current_history_estimator_revision(),
            project_breakdown_revision: HISTORY_PROJECT_BREAKDOWN_REVISION,
            api_pricing_catalog_revision: API_PRICING_CATALOG_REVISION,
            call_count: groups.len() as u64,
            groups: Vec::new(),
            project_groups: groups,
            partial_reasons: Vec::new(),
        }
    }

    fn analysis(duration_mins: i64, starts_at: DateTime<Utc>) -> WindowAnalysis {
        WindowAnalysis {
            duration_mins,
            attribution: AttributionSummary {
                window: Some(WindowDescriptor {
                    limit_id: "codex".to_owned(),
                    label: duration_mins.to_string(),
                    starts_at,
                    ends_at: at(12, 0),
                    used_percent: 10.0,
                }),
                ..AttributionSummary::default()
            },
            partial: false,
            partial_reasons: Vec::new(),
            threads: Vec::new(),
            turns: Vec::new(),
            models: Vec::new(),
            api_equivalent_cost: Default::default(),
            api_pricing: Default::default(),
            api_long_context: None,
        }
    }

    #[test]
    fn projects_missing_turns_with_distinct_windows_and_ancestor_closure() {
        let node = "node-0123456789abcdef0123456789abcdef";
        let child = format!("child@{node}");
        let parent = format!("root@{node}");
        let history = RemoteOverviewHistory::from_unified(
            &HistoryData {
                half_hour_buckets: vec![
                    bucket(
                        at(6, 0),
                        vec![group(
                            &child,
                            "old@node-0123456789abcdef0123456789abcdef",
                            Some(&parent),
                            70,
                        )],
                    ),
                    bucket(
                        at(11, 0),
                        vec![group(
                            &child,
                            "recent@node-0123456789abcdef0123456789abcdef",
                            Some(&parent),
                            30,
                        )],
                    ),
                ],
                ..HistoryData::default()
            },
            [(node.parse().unwrap(), "remote-a".to_owned())],
            at(12, 0),
        );
        let projection = project_remote_overview_history(
            &history,
            &[analysis(300, at(10, 0)), analysis(10_080, at(0, 0))],
            at(12, 0),
        );
        let five = projection
            .windows
            .iter()
            .find(|window| window.duration_mins == 300)
            .unwrap();
        let week = projection
            .windows
            .iter()
            .find(|window| window.duration_mins == 10_080)
            .unwrap();
        assert_eq!(five.threads[0].usage.token_usage.total_tokens, 30);
        assert_eq!(week.threads[0].usage.token_usage.total_tokens, 100);
        assert_eq!(projection.turns.len(), 2);
        let root_id = format!("remote:{node}:root");
        let child_id = format!("remote:{node}:child");
        assert!(
            projection
                .tasks
                .iter()
                .any(|task| task.thread_id == root_id)
        );
        let child = projection
            .tasks
            .iter()
            .find(|task| task.thread_id == child_id)
            .unwrap();
        assert_eq!(child.parent_thread_id.as_deref(), Some(root_id.as_str()));
        assert!(
            projection
                .turns
                .iter()
                .all(|turn| turn.message_preview.is_none()
                    && turn.started_at.is_none()
                    && turn.completed_at.is_none()
                    && turn.duration_ms.is_none()
                    && turn.status == TurnStatus::Unknown)
        );
        assert!(
            projection
                .tasks
                .iter()
                .all(|task| task.status == TaskStatus::Unknown)
        );
    }

    #[test]
    fn logical_replica_is_canonical_and_marks_local_row_for_replacement() {
        let history = RemoteOverviewHistory::from_unified(
            &HistoryData {
                half_hour_buckets: vec![bucket(
                    at(11, 0),
                    vec![group(
                        "logical-thread:same",
                        "turn@node-0123456789abcdef0123456789abcdef",
                        None,
                        40,
                    )],
                )],
                ..HistoryData::default()
            },
            [(
                "node-0123456789abcdef0123456789abcdef".parse().unwrap(),
                "remote-a".to_owned(),
            )],
            at(12, 0),
        );
        let projection =
            project_remote_overview_history(&history, &[analysis(300, at(10, 0))], at(12, 0));
        assert_eq!(projection.windows[0].threads[0].thread_id, "same");
        assert!(
            projection.windows[0]
                .replaced_local_threads
                .contains("same")
        );
    }

    #[test]
    fn unified_models_add_an_explicit_unknown_residual_without_adding_remote_threads_twice() {
        let remote_node = "node-0123456789abcdef0123456789abcdef";
        let local_node = "node-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let mut unified_bucket = bucket(
            at(11, 0),
            vec![
                group(
                    "logical-thread:same",
                    &format!("remote-turn@{remote_node}"),
                    None,
                    40,
                ),
                group(
                    &format!("local-only@{local_node}"),
                    &format!("local-turn@{local_node}"),
                    None,
                    60,
                ),
            ],
        );
        unified_bucket.api_long_context_extra_cost_units = Some(10);
        unified_bucket.groups = vec![LocalUsageGroup {
            model: Some("gpt-5.6-sol".to_owned()),
            service_tier: Some("standard".to_owned()),
            token_usage: usage(70),
            estimated_cost_units: 70,
            api_long_context_extra_cost_units: Some(7),
            api_equivalent_cost: ApiCostAmount {
                observed_samples: 1,
                priced_samples: 1,
                observed_tokens: 70,
                priced_tokens: 70,
                ..ApiCostAmount::default()
            },
            api_equivalent_cost_complete: true,
            call_count: 1,
            ..LocalUsageGroup::default()
        }];
        let history = RemoteOverviewHistory::from_unified(
            &HistoryData {
                half_hour_buckets: vec![unified_bucket],
                ..HistoryData::default()
            },
            [(remote_node.parse().unwrap(), "remote-a".to_owned())],
            at(12, 0),
        );

        let projection =
            project_remote_overview_history(&history, &[analysis(300, at(10, 0))], at(12, 0));
        let window = &projection.windows[0];
        let models = window.models.as_ref().unwrap();
        assert_eq!(window.model_token_usage.total_tokens, 100);
        assert_eq!(
            models
                .iter()
                .map(|model| model.token_usage.total_tokens)
                .sum::<u64>(),
            100
        );
        assert_eq!(
            models
                .iter()
                .find(|model| model.model == "gpt-5.6-sol")
                .unwrap()
                .token_usage
                .total_tokens,
            70
        );
        assert_eq!(
            models
                .iter()
                .find(|model| model.model == "unknown")
                .unwrap()
                .token_usage
                .total_tokens,
            30
        );
        assert!(
            window
                .partial_reasons
                .contains(&"remote_model_breakdown_partial".to_owned())
        );
    }

    #[test]
    fn configured_remote_without_window_usage_keeps_the_live_local_models_projection() {
        let remote_node = "node-0123456789abcdef0123456789abcdef";
        let local_node = "node-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let mut local_bucket = bucket(
            at(11, 0),
            vec![group(
                &format!("local-only@{local_node}"),
                &format!("local-turn@{local_node}"),
                None,
                60,
            )],
        );
        local_bucket.groups = vec![LocalUsageGroup {
            model: Some("gpt-5.6-sol".to_owned()),
            token_usage: usage(60),
            estimated_cost_units: 60,
            api_long_context_extra_cost_units: Some(0),
            call_count: 1,
            ..LocalUsageGroup::default()
        }];
        let history = RemoteOverviewHistory::from_unified(
            &HistoryData {
                half_hour_buckets: vec![local_bucket],
                ..HistoryData::default()
            },
            [(remote_node.parse().unwrap(), "remote-a".to_owned())],
            at(12, 0),
        );

        let mut local_analysis = analysis(300, at(10, 0));
        local_analysis.attribution.local_token_usage = usage(60);
        let projection = project_remote_overview_history(&history, &[local_analysis], at(12, 0));

        assert!(projection.windows[0].threads.is_empty());
        assert!(projection.windows[0].models.is_none());
    }

    #[test]
    fn opaque_all_source_residual_still_replaces_the_local_models_projection() {
        let remote_node = "node-0123456789abcdef0123456789abcdef";
        let mut opaque_bucket = bucket(at(11, 0), Vec::new());
        let opaque_tokens = TokenUsage {
            input_tokens: 100,
            output_tokens: 20,
            total_tokens: 120,
            ..TokenUsage::default()
        };
        opaque_bucket.token_usage = opaque_tokens;
        opaque_bucket.estimated_cost_units = 120;
        opaque_bucket.call_count = 1;
        opaque_bucket.groups = vec![LocalUsageGroup {
            model: Some("gpt-5.6-sol".to_owned()),
            token_usage: opaque_tokens,
            estimated_cost_units: 120,
            api_long_context_extra_cost_units: Some(0),
            api_equivalent_cost: ApiCostAmount {
                observed_samples: 1,
                priced_samples: 1,
                observed_tokens: 120,
                priced_tokens: 120,
                ..ApiCostAmount::default()
            },
            api_equivalent_cost_complete: true,
            call_count: 1,
            ..LocalUsageGroup::default()
        }];
        let history = RemoteOverviewHistory::from_unified(
            &HistoryData {
                half_hour_buckets: vec![opaque_bucket],
                ..HistoryData::default()
            },
            [(remote_node.parse().unwrap(), "remote-a".to_owned())],
            at(12, 0),
        );
        let mut local_analysis = analysis(300, at(10, 0));
        local_analysis.attribution.local_token_usage = TokenUsage {
            input_tokens: 110,
            total_tokens: 110,
            ..TokenUsage::default()
        };

        let projection = project_remote_overview_history(&history, &[local_analysis], at(12, 0));
        let window = &projection.windows[0];

        assert!(window.threads.is_empty());
        assert_eq!(window.model_token_usage.total_tokens, 120);
        assert_eq!(
            window.models.as_ref().unwrap()[0].token_usage.total_tokens,
            120
        );
        assert!(
            window
                .partial_reasons
                .contains(&"remote_project_breakdown_unavailable".to_owned())
        );
        assert!(window.api_equivalent_cost.as_ref().unwrap().is_partial());
    }

    #[test]
    fn excluded_boundary_bucket_marks_the_unified_api_total_as_a_lower_bound() {
        let node = "node-0123456789abcdef0123456789abcdef";
        let remote_group = |turn: &str, tokens| {
            group(
                &format!("thread@{node}"),
                &format!("{turn}@{node}"),
                None,
                tokens,
            )
        };
        let history = RemoteOverviewHistory::from_unified(
            &HistoryData {
                half_hour_buckets: vec![
                    bucket(at(9, 50), vec![remote_group("edge", 20)]),
                    bucket(at(11, 0), vec![remote_group("inside", 40)]),
                ],
                ..HistoryData::default()
            },
            [(node.parse().unwrap(), "remote-a".to_owned())],
            at(12, 0),
        );

        let projection =
            project_remote_overview_history(&history, &[analysis(300, at(10, 0))], at(12, 0));
        let api = projection.windows[0].api_equivalent_cost.as_ref().unwrap();

        assert!(
            api.partial_reasons
                .contains(&"remote_window_boundary_lower_bound".to_owned())
        );
        assert!(api.is_partial());
    }

    #[test]
    fn rollout_coverage_gap_marks_api_total_partial_but_project_warning_does_not() {
        let node = "node-0123456789abcdef0123456789abcdef";
        let remote_group = group(&format!("thread@{node}"), &format!("turn@{node}"), None, 40);
        let mut partial_bucket = bucket(at(11, 0), vec![remote_group.clone()]);
        partial_bucket.partial_reasons = vec!["rollout_scan_incomplete".to_owned()];
        let partial_history = RemoteOverviewHistory::from_unified(
            &HistoryData {
                half_hour_buckets: vec![partial_bucket],
                ..HistoryData::default()
            },
            [(node.parse().unwrap(), "remote-a".to_owned())],
            at(12, 0),
        );
        let partial = project_remote_overview_history(
            &partial_history,
            &[analysis(300, at(10, 0))],
            at(12, 0),
        );
        let partial_api = partial.windows[0].api_equivalent_cost.as_ref().unwrap();
        assert!(
            partial_api
                .partial_reasons
                .contains(&"remote_api_history_coverage_partial".to_owned())
        );

        let project_warning_history = RemoteOverviewHistory::from_unified(
            &HistoryData {
                half_hour_buckets: vec![bucket(at(11, 0), vec![remote_group])],
                warnings: vec!["project_mapping_partial".to_owned()],
                ..HistoryData::default()
            },
            [(node.parse().unwrap(), "remote-a".to_owned())],
            at(12, 0),
        );
        let project_warning = project_remote_overview_history(
            &project_warning_history,
            &[analysis(300, at(10, 0))],
            at(12, 0),
        );
        let project_warning_api = project_warning.windows[0]
            .api_equivalent_cost
            .as_ref()
            .unwrap();
        assert!(!project_warning_api.is_partial());
    }

    #[test]
    fn external_estimator_revision_is_excluded_from_all_source_est_and_longx() {
        let node = "node-0123456789abcdef0123456789abcdef";
        let model_group = |tokens, estimated_cost_units, long_extra| LocalUsageGroup {
            model: Some("gpt-5.6-sol".to_owned()),
            token_usage: usage(tokens),
            estimated_cost_units,
            api_long_context_extra_cost_units: Some(long_extra),
            api_equivalent_cost: ApiCostAmount {
                observed_samples: 1,
                priced_samples: 1,
                observed_tokens: tokens,
                priced_tokens: tokens,
                ..ApiCostAmount::default()
            },
            api_equivalent_cost_complete: true,
            call_count: 1,
            ..LocalUsageGroup::default()
        };

        let mut current_bucket = bucket(
            at(10, 30),
            vec![group("local-thread", "local-turn", None, 40)],
        );
        current_bucket.estimated_cost_units = 400;
        current_bucket.api_long_context_extra_cost_units = Some(40);
        current_bucket.groups = vec![model_group(40, 400, 40)];

        let mut external_bucket = bucket(
            at(11, 0),
            vec![group(
                &format!("remote-thread@{node}"),
                &format!("remote-turn@{node}"),
                None,
                60,
            )],
        );
        external_bucket.estimator_revision = current_history_estimator_revision().saturating_add(1);
        external_bucket.estimated_cost_units = 60_000;
        external_bucket.api_long_context_extra_cost_units = Some(6_000);
        external_bucket.groups = vec![model_group(60, 60_000, 6_000)];

        let history = RemoteOverviewHistory::from_unified(
            &HistoryData {
                half_hour_buckets: vec![current_bucket, external_bucket],
                ..HistoryData::default()
            },
            [(node.parse().unwrap(), "remote-a".to_owned())],
            at(12, 0),
        );
        let projection =
            project_remote_overview_history(&history, &[analysis(300, at(10, 0))], at(12, 0));
        let window = &projection.windows[0];
        let model = window
            .models
            .as_ref()
            .unwrap()
            .iter()
            .find(|model| model.model == "gpt-5.6-sol")
            .unwrap();

        assert_eq!(window.model_token_usage.total_tokens, 100);
        assert_eq!(window.estimated_cost_units, 400);
        assert_eq!(window.api_long_context_extra_cost_units, Some(40));
        assert_eq!(model.token_usage.total_tokens, 100);
        assert_eq!(model.estimated_cost_units, 400);
        assert_eq!(model.api_long_context_extra_cost_units, Some(40));
        assert!(
            window
                .partial_reasons
                .contains(&"remote_estimator_revision_mismatch".to_owned())
        );
    }

    #[test]
    fn outdated_remote_revisions_are_partial_and_api_cost_becomes_unpriced() {
        let node = "node-0123456789abcdef0123456789abcdef";
        let mut old_bucket = bucket(
            at(11, 0),
            vec![group(
                &format!("thread@{node}"),
                &format!("turn@{node}"),
                None,
                40,
            )],
        );
        old_bucket.project_breakdown_revision =
            HISTORY_PROJECT_BREAKDOWN_REVISION.saturating_sub(1);
        old_bucket.api_pricing_catalog_revision = API_PRICING_CATALOG_REVISION.saturating_sub(1);
        let history_data = HistoryData {
            half_hour_buckets: vec![old_bucket],
            ..HistoryData::default()
        };
        let history = RemoteOverviewHistory::from_unified(
            &history_data,
            [(node.parse().unwrap(), "remote-a".to_owned())],
            at(12, 0),
        );

        let projection =
            project_remote_overview_history(&history, &[analysis(300, at(10, 0))], at(12, 0));
        let window = &projection.windows[0];
        let amount = window.threads[0].usage.api_equivalent_cost;
        assert_eq!(amount.observed_tokens, 40);
        assert_eq!(amount.priced_tokens, 0);
        assert_eq!(amount.priced_samples, 0);
        assert!(
            window
                .partial_reasons
                .contains(&"remote_project_breakdown_revision_mismatch".to_owned())
        );
        assert!(
            window
                .partial_reasons
                .contains(&"remote_api_pricing_catalog_revision_mismatch".to_owned())
        );
    }

    #[test]
    fn conflicting_remote_lineage_drops_the_edge_and_marks_the_window_partial() {
        let node = "node-0123456789abcdef0123456789abcdef";
        let child = format!("child@{node}");
        let first_parent = format!("first-parent@{node}");
        let second_parent = format!("second-parent@{node}");
        let history_data = HistoryData {
            half_hour_buckets: vec![
                bucket(
                    at(10, 30),
                    vec![group(
                        &child,
                        &format!("turn-a@{node}"),
                        Some(&first_parent),
                        10,
                    )],
                ),
                bucket(
                    at(11, 0),
                    vec![group(
                        &child,
                        &format!("turn-b@{node}"),
                        Some(&second_parent),
                        20,
                    )],
                ),
            ],
            ..HistoryData::default()
        };
        let history = RemoteOverviewHistory::from_unified(
            &history_data,
            [(node.parse().unwrap(), "remote-a".to_owned())],
            at(12, 0),
        );

        let projection =
            project_remote_overview_history(&history, &[analysis(300, at(10, 0))], at(12, 0));
        let child_id = format!("remote:{node}:child");
        let child = projection
            .tasks
            .iter()
            .find(|task| task.thread_id == child_id)
            .unwrap();
        assert_eq!(child.parent_thread_id, None);
        assert!(
            projection.windows[0]
                .partial_reasons
                .contains(&"remote_history_lineage_conflict".to_owned())
        );
    }
}
