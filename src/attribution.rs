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

struct SelectedWindow<'a> {
    bucket: &'a LimitBucket,
    window: &'a LimitWindow,
    starts_at: DateTime<Utc>,
    ends_at: DateTime<Utc>,
}

/// Calculates local-token shares and low-confidence quota estimates for the
/// current normal Codex five-hour and weekly rate-limit windows.
///
/// Spark calls are excluded. Every remaining estimate uses one stable formula:
/// current Codex used percent multiplied by the entity's local token share.
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

    let mut local_token_usage = TokenUsage::default();
    for call in &window_calls {
        local_token_usage.add_assign(call.tokens);
    }

    let total_tokens = local_token_usage.total_tokens;
    let mut task_tokens: BTreeMap<String, TokenUsage> = BTreeMap::new();
    let mut turn_tokens: BTreeMap<(String, String), TokenUsage> = BTreeMap::new();
    let mut model_tokens: BTreeMap<String, TokenUsage> = BTreeMap::new();

    for call in &window_calls {
        task_tokens
            .entry(call.thread_id.clone())
            .or_default()
            .add_assign(call.tokens);
        if let Some(turn_id) = &call.turn_id {
            turn_tokens
                .entry((call.thread_id.clone(), turn_id.clone()))
                .or_default()
                .add_assign(call.tokens);
        }
        model_tokens
            .entry(model_name(call))
            .or_default()
            .add_assign(call.tokens);
    }

    let used_percent = selected.window.used_percent.clamp(0.0, 100.0);
    let confidence = if total_tokens == 0 {
        Confidence::Unknown
    } else {
        Confidence::Low
    };
    let usage_for = |token_usage: TokenUsage| {
        let local_token_share_percent = token_share(token_usage, total_tokens);
        WindowUsage {
            token_usage,
            local_token_share_percent,
            estimated_quota_percent: used_percent * local_token_share_percent / 100.0,
            quota_confidence: confidence,
        }
    };

    let threads = task_tokens
        .into_iter()
        .map(|(thread_id, token_usage)| ThreadWindowUsage {
            thread_id,
            usage: usage_for(token_usage),
        })
        .collect();
    let turns = turn_tokens
        .into_iter()
        .map(|((thread_id, turn_id), token_usage)| TurnWindowUsage {
            thread_id,
            turn_id,
            usage: usage_for(token_usage),
        })
        .collect();
    let models = model_tokens
        .into_iter()
        .map(|(model, token_usage)| ModelUsage {
            local_token_share_percent: token_share(token_usage, total_tokens),
            estimated_quota_percent: used_percent * token_share(token_usage, total_tokens) / 100.0,
            quota_confidence: confidence,
            model,
            token_usage,
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
            "current_codex_gauge_token_share_proxy".to_string()
        },
        settled,
    };

    WindowAnalysis {
        duration_mins: selected
            .window
            .window_duration_mins
            .expect("selected windows always have a duration"),
        partial: quota_window_stale,
        partial_reasons: if quota_window_stale {
            vec!["quota_window_stale".to_string()]
        } else {
            Vec::new()
        },
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

fn is_spark_model(model: Option<&str>) -> bool {
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

fn model_name(call: &UsageCall) -> String {
    call.model
        .as_deref()
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .unwrap_or("unknown")
        .to_string()
}
