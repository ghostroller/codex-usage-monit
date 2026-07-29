use std::collections::BTreeMap;

use chrono::{DateTime, Duration, Utc};

use crate::domain::{
    AttributionSummary, Confidence, LimitBucket, LimitWindow, ModelUsage, Provenance,
    RateObservation, TaskRecord, ThreadWindowUsage, TokenUsage, TurnRecord, TurnWindowUsage,
    UsageCall, WindowAnalysis, WindowDescriptor, WindowUsage,
};

const FIVE_HOURS_MINS: i64 = 300;
const WEEK_MINS: i64 = 10_080;
const ANALYZED_WINDOW_DURATIONS: [i64; 2] = [FIVE_HOURS_MINS, WEEK_MINS];
const RESET_DRIFT_SECS: i64 = 120;
const DEFAULT_CODEX_BUCKET: &str = "codex";
const SPARK_MODEL: &str = "gpt-5.3-codex-spark";
pub(crate) const ESTIMATOR_REVISION: u32 = 1;

// OpenAI short-context prices as of 2026-07-14:
// https://developers.openai.com/api/docs/pricing?latest-pricing=priority
// Integer rates use $0.025 per million tokens. The currency and per-million
// scales cancel when the values are converted into relative shares.
#[derive(Clone, Copy)]
struct TokenRates {
    input: u128,
    cached_input: u128,
    output: u128,
}

const SOL_STANDARD: TokenRates = TokenRates::new(200, 20, 1_200);
const SOL_FAST: TokenRates = TokenRates::new(400, 40, 2_400);
const TERRA_STANDARD: TokenRates = TokenRates::new(100, 10, 600);
const TERRA_FAST: TokenRates = TokenRates::new(200, 20, 1_200);
const LUNA_STANDARD: TokenRates = TokenRates::new(40, 4, 240);
const LUNA_FAST: TokenRates = TokenRates::new(80, 8, 480);
const GPT_5_5_STANDARD: TokenRates = TokenRates::new(200, 20, 1_200);
const GPT_5_5_FAST: TokenRates = TokenRates::new(500, 50, 3_000);
const GPT_5_4_STANDARD: TokenRates = TokenRates::new(100, 10, 600);
const GPT_5_4_FAST: TokenRates = TokenRates::new(200, 20, 1_200);
const GPT_5_4_MINI_STANDARD: TokenRates = TokenRates::new(30, 3, 180);
const GPT_5_4_MINI_FAST: TokenRates = TokenRates::new(60, 6, 360);

impl TokenRates {
    const fn new(input: u128, cached_input: u128, output: u128) -> Self {
        Self {
            input,
            cached_input,
            output,
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct EstimatedUsageWeight {
    pub(crate) units: u128,
    pub(crate) used_model_fallback: bool,
    pub(crate) used_token_breakdown_fallback: bool,
}

#[derive(Clone, Copy, Default)]
struct UsageAccumulator {
    tokens: TokenUsage,
    estimated_cost_units: u128,
}

impl UsageAccumulator {
    fn add_call(&mut self, call: &UsageCall, estimated_cost_units: u128) {
        self.tokens.add_assign(call.tokens);
        self.estimated_cost_units = self
            .estimated_cost_units
            .saturating_add(estimated_cost_units);
    }
}

struct SelectedWindow<'a> {
    bucket: &'a LimitBucket,
    window: &'a LimitWindow,
    starts_at: DateTime<Utc>,
    ends_at: DateTime<Utc>,
}

/// Calculates raw local-token shares and low-confidence quota estimates for
/// the current normal Codex five-hour and weekly rate-limit windows.
///
/// Spark calls are excluded. EST uses the published short-context Standard or
/// Fast token prices, while raw token totals and TOKEN% shares remain unchanged.
pub fn analyze_windows(
    tasks: &[TaskRecord],
    _turns: &[TurnRecord],
    calls: &[UsageCall],
    _observations: &[RateObservation],
    limits: &[LimitBucket],
    now: DateTime<Utc>,
) -> Vec<WindowAnalysis> {
    let settled = is_settled(tasks);
    select_windows(limits, now)
        .into_iter()
        .map(|selected| analyze_selected_window(calls, selected, now, settled))
        .collect()
}

/// Preserves the original five-hour API and projects its result onto the legacy
/// per-task, per-turn, model and attribution fields.
pub fn analyze_current_window(
    tasks: &mut [TaskRecord],
    turns: &mut [TurnRecord],
    calls: &[UsageCall],
    observations: &[RateObservation],
    limits: &[LimitBucket],
    now: DateTime<Utc>,
) -> (Vec<ModelUsage>, AttributionSummary) {
    let analyses = analyze_windows(tasks, turns, calls, observations, limits, now);
    project_five_hour_analysis(tasks, turns, &analyses)
}

pub(crate) fn project_five_hour_analysis(
    tasks: &mut [TaskRecord],
    turns: &mut [TurnRecord],
    analyses: &[WindowAnalysis],
) -> (Vec<ModelUsage>, AttributionSummary) {
    reset_records(tasks, turns);
    let Some(analysis) = analyses.iter().find(|analysis| {
        analysis.duration_mins == FIVE_HOURS_MINS
            && analysis.attribution.window.as_ref().is_some_and(|window| {
                window
                    .limit_id
                    .trim()
                    .eq_ignore_ascii_case(DEFAULT_CODEX_BUCKET)
            })
    }) else {
        return (
            Vec::new(),
            AttributionSummary {
                settled: is_settled(tasks),
                ..AttributionSummary::default()
            },
        );
    };

    let thread_usage = analysis
        .threads
        .iter()
        .map(|thread| (thread.thread_id.as_str(), thread.usage))
        .collect::<BTreeMap<_, _>>();
    let turn_usage = analysis
        .turns
        .iter()
        .map(|turn| ((turn.thread_id.as_str(), turn.turn_id.as_str()), turn.usage))
        .collect::<BTreeMap<_, _>>();

    for task in tasks {
        if let Some(usage) = thread_usage.get(task.thread_id.as_str()).copied() {
            task.window_token_usage = usage.token_usage;
            task.local_token_share_percent = usage.local_token_share_percent;
            task.estimated_quota_percent = usage.estimated_quota_percent;
            task.quota_confidence = usage.quota_confidence;
        }
    }
    for turn in turns {
        if let Some(usage) = turn_usage
            .get(&(turn.thread_id.as_str(), turn.turn_id.as_str()))
            .copied()
        {
            turn.window_token_usage = usage.token_usage;
            turn.local_token_share_percent = usage.local_token_share_percent;
            turn.estimated_quota_percent = usage.estimated_quota_percent;
            turn.quota_confidence = usage.quota_confidence;
        }
    }

    (analysis.models.clone(), analysis.attribution.clone())
}

fn is_settled(tasks: &[TaskRecord]) -> bool {
    tasks.iter().all(|task| {
        matches!(
            task.status,
            crate::domain::TaskStatus::Idle
                | crate::domain::TaskStatus::Completed
                | crate::domain::TaskStatus::Interrupted
                | crate::domain::TaskStatus::Failed
        )
    })
}

fn analyze_selected_window(
    calls: &[UsageCall],
    selected: SelectedWindow<'_>,
    now: DateTime<Utc>,
    settled: bool,
) -> WindowAnalysis {
    let quota_window_stale = matches!(
        selected.bucket.provenance,
        Provenance::Stale | Provenance::Unknown
    );
    let window_calls = calls
        .iter()
        .filter(|call| {
            call.timestamp >= selected.starts_at
                && call.timestamp <= now
                && call.timestamp <= selected.ends_at
                && !is_spark_model(call.model.as_deref())
        })
        .collect::<Vec<_>>();

    let mut local_usage = UsageAccumulator::default();
    let mut task_usage: BTreeMap<String, UsageAccumulator> = BTreeMap::new();
    let mut turn_usage: BTreeMap<(String, String), UsageAccumulator> = BTreeMap::new();
    let mut model_usage: BTreeMap<String, UsageAccumulator> = BTreeMap::new();
    let mut used_model_fallback = false;
    let mut used_token_breakdown_fallback = false;

    for call in &window_calls {
        let estimated_cost = estimate_call_weight(call);
        used_model_fallback |= estimated_cost.used_model_fallback;
        used_token_breakdown_fallback |= estimated_cost.used_token_breakdown_fallback;
        local_usage.add_call(call, estimated_cost.units);
        task_usage
            .entry(call.thread_id.clone())
            .or_default()
            .add_call(call, estimated_cost.units);
        if let Some(turn_id) = &call.turn_id {
            turn_usage
                .entry((call.thread_id.clone(), turn_id.clone()))
                .or_default()
                .add_call(call, estimated_cost.units);
        }
        model_usage
            .entry(model_name(call))
            .or_default()
            .add_call(call, estimated_cost.units);
    }

    let local_token_usage = local_usage.tokens;
    let total_tokens = local_token_usage.total_tokens;
    let total_estimated_cost_units = local_usage.estimated_cost_units;

    let used_percent = selected.window.used_percent.clamp(0.0, 100.0);
    let confidence = if total_tokens == 0 {
        Confidence::Unknown
    } else {
        Confidence::Low
    };
    let usage_for = |usage: UsageAccumulator| {
        let local_token_share_percent = token_share(usage.tokens, total_tokens);
        WindowUsage {
            token_usage: usage.tokens,
            local_token_share_percent,
            estimated_quota_percent: used_percent
                * cost_share(usage.estimated_cost_units, total_estimated_cost_units)
                / 100.0,
            quota_confidence: confidence,
        }
    };

    let threads = task_usage
        .into_iter()
        .map(|(thread_id, usage)| ThreadWindowUsage {
            thread_id,
            usage: usage_for(usage),
        })
        .collect();
    let turns = turn_usage
        .into_iter()
        .map(|((thread_id, turn_id), usage)| TurnWindowUsage {
            thread_id,
            turn_id,
            usage: usage_for(usage),
        })
        .collect();
    let models = model_usage
        .into_iter()
        .map(|(model, usage)| ModelUsage {
            local_token_share_percent: token_share(usage.tokens, total_tokens),
            estimated_quota_percent: used_percent
                * cost_share(usage.estimated_cost_units, total_estimated_cost_units)
                / 100.0,
            quota_confidence: confidence,
            model,
            token_usage: usage.tokens,
        })
        .collect();

    let summary = AttributionSummary {
        window: Some(WindowDescriptor {
            limit_id: selected.bucket.limit_id.clone(),
            label: selected.window.label(),
            starts_at: selected.starts_at,
            ends_at: selected.ends_at,
            used_percent,
        }),
        local_token_usage,
        observed_delta_percent: 0.0,
        estimated_assigned_percent: 0.0,
        proxy_projected_percent: if total_tokens == 0 { 0.0 } else { used_percent },
        unattributed_percent: used_percent,
        attribution_coverage_percent: 0.0,
        external_activity_possible: true,
        confidence,
        method: if total_tokens == 0 {
            "codex_gauge_without_local_tokens".to_string()
        } else {
            "current_codex_gauge_short_context_price_weighted_proxy".to_string()
        },
        settled,
    };

    let mut partial_reasons = Vec::new();
    if quota_window_stale {
        partial_reasons.push("quota_window_stale".to_string());
    }
    if used_model_fallback {
        partial_reasons.push("unpriced_model_rate_fallback".to_string());
    }
    if used_token_breakdown_fallback {
        partial_reasons.push("token_breakdown_missing".to_string());
    }

    WindowAnalysis {
        duration_mins: selected
            .window
            .window_duration_mins
            .expect("selected windows always have a duration"),
        partial: !partial_reasons.is_empty(),
        partial_reasons,
        attribution: summary,
        threads,
        turns,
        models,
    }
}

fn reset_records(tasks: &mut [TaskRecord], turns: &mut [TurnRecord]) {
    for task in tasks {
        task.window_token_usage = TokenUsage::default();
        task.local_token_share_percent = 0.0;
        task.estimated_quota_percent = 0.0;
        task.quota_confidence = Confidence::Unknown;
    }
    for turn in turns {
        turn.window_token_usage = TokenUsage::default();
        turn.local_token_share_percent = 0.0;
        turn.estimated_quota_percent = 0.0;
        turn.quota_confidence = Confidence::Unknown;
    }
}

fn select_windows(limits: &[LimitBucket], now: DateTime<Utc>) -> Vec<SelectedWindow<'_>> {
    let mut usable = Vec::new();
    for bucket in limits {
        if let Some(window) = &bucket.primary
            && let Some(selected) = usable_window(bucket, window)
        {
            usable.push(selected);
        }
        if let Some(window) = &bucket.secondary
            && let Some(selected) = usable_window(bucket, window)
        {
            usable.push(selected);
        }
    }

    usable.retain(|window| {
        window
            .bucket
            .limit_id
            .trim()
            .eq_ignore_ascii_case(DEFAULT_CODEX_BUCKET)
            && window
                .window
                .window_duration_mins
                .is_some_and(|duration| ANALYZED_WINDOW_DURATIONS.contains(&duration))
            && is_current_window(window, now)
    });
    usable.sort_by(|left, right| {
        left.window
            .window_duration_mins
            .cmp(&right.window.window_duration_mins)
            .then_with(|| bucket_priority(left.bucket).cmp(&bucket_priority(right.bucket)))
            .then_with(|| right.bucket.as_of.cmp(&left.bucket.as_of))
            .then_with(|| left.ends_at.cmp(&right.ends_at))
    });
    let mut selected: Vec<SelectedWindow<'_>> = Vec::new();
    for window in usable {
        let already_selected = selected.iter().any(|previous| {
            previous.window.window_duration_mins == window.window.window_duration_mins
        });
        if !already_selected {
            selected.push(window);
        }
    }
    selected
}

fn bucket_priority(bucket: &LimitBucket) -> u8 {
    match bucket.provenance {
        Provenance::ServerSnapshot => 0,
        Provenance::Live => 1,
        Provenance::LocalExact => 2,
        Provenance::Inferred => 3,
        Provenance::Estimated => 4,
        Provenance::Stale => 5,
        Provenance::Unknown => 6,
    }
}

pub(crate) fn is_spark_model(model: Option<&str>) -> bool {
    model.is_some_and(|model| model.trim().eq_ignore_ascii_case(SPARK_MODEL))
}

fn is_current_window(window: &SelectedWindow<'_>, now: DateTime<Utc>) -> bool {
    now >= window.starts_at - Duration::seconds(RESET_DRIFT_SECS) && now < window.ends_at
}

fn usable_window<'a>(
    bucket: &'a LimitBucket,
    window: &'a LimitWindow,
) -> Option<SelectedWindow<'a>> {
    let duration = window.window_duration_mins.filter(|minutes| *minutes > 0)?;
    let ends_at = window.resets_at?;
    Some(SelectedWindow {
        bucket,
        window,
        starts_at: ends_at - Duration::minutes(duration),
        ends_at,
    })
}

fn token_share(tokens: TokenUsage, total_tokens: u64) -> f64 {
    if total_tokens == 0 {
        0.0
    } else {
        tokens.total_tokens as f64 / total_tokens as f64 * 100.0
    }
}

fn cost_share(estimated_cost_units: u128, total_estimated_cost_units: u128) -> f64 {
    if total_estimated_cost_units == 0 {
        0.0
    } else {
        estimated_cost_units as f64 / total_estimated_cost_units as f64 * 100.0
    }
}

pub(crate) fn estimate_call_weight(call: &UsageCall) -> EstimatedUsageWeight {
    let rates = short_context_rates(call.model.as_deref(), call.is_fast());
    let used_model_fallback = rates.is_none() && !call.tokens.is_zero();
    let rates = rates.unwrap_or(if call.is_fast() {
        LUNA_FAST
    } else {
        LUNA_STANDARD
    });

    let tokens = call.tokens;
    let used_token_breakdown_fallback =
        tokens.total_tokens > 0 && tokens.input_tokens == 0 && tokens.output_tokens == 0;
    let (input_tokens, cached_input_tokens, output_tokens) = if used_token_breakdown_fallback {
        (tokens.total_tokens, 0, 0)
    } else {
        let cached_input_tokens = tokens.cached_input_tokens.min(tokens.input_tokens);
        (
            tokens.input_tokens - cached_input_tokens,
            cached_input_tokens,
            tokens.output_tokens,
        )
    };

    let units = u128::from(input_tokens)
        .saturating_mul(rates.input)
        .saturating_add(u128::from(cached_input_tokens).saturating_mul(rates.cached_input))
        .saturating_add(u128::from(output_tokens).saturating_mul(rates.output));

    EstimatedUsageWeight {
        units,
        used_model_fallback,
        used_token_breakdown_fallback,
    }
}

fn short_context_rates(model: Option<&str>, fast: bool) -> Option<TokenRates> {
    let model = model?.trim();
    let (standard, priority) = if model.eq_ignore_ascii_case("gpt-5.6-sol") {
        (SOL_STANDARD, SOL_FAST)
    } else if model.eq_ignore_ascii_case("gpt-5.6-terra") {
        (TERRA_STANDARD, TERRA_FAST)
    } else if model.eq_ignore_ascii_case("gpt-5.6-luna") {
        (LUNA_STANDARD, LUNA_FAST)
    } else if model.eq_ignore_ascii_case("gpt-5.5") {
        (GPT_5_5_STANDARD, GPT_5_5_FAST)
    } else if model.eq_ignore_ascii_case("gpt-5.4") {
        (GPT_5_4_STANDARD, GPT_5_4_FAST)
    } else if model.eq_ignore_ascii_case("gpt-5.4-mini") {
        (GPT_5_4_MINI_STANDARD, GPT_5_4_MINI_FAST)
    } else {
        return None;
    };
    Some(if fast { priority } else { standard })
}

fn model_name(call: &UsageCall) -> String {
    call.model
        .as_deref()
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .unwrap_or("unknown")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn priced_call(model: &str, fast: bool, tokens: TokenUsage) -> UsageCall {
        UsageCall {
            timestamp: Utc::now(),
            thread_id: "thread".to_string(),
            turn_id: Some("turn".to_string()),
            model: Some(model.to_string()),
            service_tier: fast.then(|| "priority".to_string()),
            tokens,
        }
    }

    #[test]
    fn published_short_context_price_matrix_uses_each_model_and_tier() {
        let tokens = TokenUsage {
            input_tokens: 13,
            cached_input_tokens: 3,
            output_tokens: 2,
            reasoning_output_tokens: 1,
            total_tokens: 999,
        };
        let cases = [
            ("gpt-5.6-sol", 4_460, 8_920),
            ("gpt-5.6-terra", 2_230, 4_460),
            ("gpt-5.6-luna", 892, 1_784),
            ("gpt-5.5", 4_460, 11_150),
            ("gpt-5.4", 2_230, 4_460),
            ("gpt-5.4-mini", 669, 1_338),
        ];

        for (model, standard, fast) in cases {
            let standard_cost = estimate_call_weight(&priced_call(model, false, tokens));
            assert_eq!(standard_cost.units, standard, "{model} Standard");
            assert!(!standard_cost.used_model_fallback);
            assert!(!standard_cost.used_token_breakdown_fallback);

            let fast_cost = estimate_call_weight(&priced_call(model, true, tokens));
            assert_eq!(fast_cost.units, fast, "{model} Fast");
            assert!(!fast_cost.used_model_fallback);
            assert!(!fast_cost.used_token_breakdown_fallback);
        }
    }

    #[test]
    fn pricing_fallbacks_are_explicit_and_keep_a_nonzero_denominator() {
        let unknown = priced_call(
            "unknown-model",
            true,
            TokenUsage {
                input_tokens: 10,
                total_tokens: 10,
                ..TokenUsage::default()
            },
        );
        let unknown_cost = estimate_call_weight(&unknown);
        assert_eq!(unknown_cost.units, 800);
        assert!(unknown_cost.used_model_fallback);
        assert!(!unknown_cost.used_token_breakdown_fallback);

        let total_only = priced_call(
            "gpt-5.6-luna",
            false,
            TokenUsage {
                total_tokens: 7,
                ..TokenUsage::default()
            },
        );
        let total_only_cost = estimate_call_weight(&total_only);
        assert_eq!(total_only_cost.units, 280);
        assert!(!total_only_cost.used_model_fallback);
        assert!(total_only_cost.used_token_breakdown_fallback);
    }

    #[test]
    fn cached_input_is_clamped_to_input_tokens() {
        let cost = estimate_call_weight(&priced_call(
            "gpt-5.6-luna",
            false,
            TokenUsage {
                input_tokens: 5,
                cached_input_tokens: 8,
                total_tokens: 5,
                ..TokenUsage::default()
            },
        ));

        assert_eq!(cost.units, 20);
    }
}
