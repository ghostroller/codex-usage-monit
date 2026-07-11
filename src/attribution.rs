use std::collections::BTreeMap;

use chrono::{DateTime, Duration, Utc};

use crate::domain::{
    AttributionSummary, Confidence, LimitBucket, LimitWindow, ModelUsage, Provenance,
    RateObservation, TaskRecord, TokenUsage, TurnRecord, UsageCall, WindowDescriptor,
};

const FIVE_HOURS_MINS: i64 = 300;
const RESET_DRIFT_SECS: i64 = 120;
const PERCENT_EPSILON: f64 = 0.01;
const MAX_ATTRIBUTION_GAP_SECS: i64 = 300;

#[derive(Clone, Copy)]
enum WindowSlot {
    Primary,
    Secondary,
}

struct SelectedWindow<'a> {
    bucket: &'a LimitBucket,
    window: &'a LimitWindow,
    slot: WindowSlot,
    starts_at: DateTime<Utc>,
    ends_at: DateTime<Utc>,
}

#[derive(Clone, Copy)]
struct QuotaSample {
    timestamp: DateTime<Utc>,
    used_percent: f64,
    provenance: Provenance,
    is_current: bool,
}

/// Calculates exact local-token shares and conservative quota estimates for the
/// current rate-limit window.
///
/// Quota estimates are derived only from positive changes between compatible
/// account snapshots. Each observed change is split across local calls made
/// since the preceding snapshot in proportion to their total token counts.
pub fn analyze_current_window(
    tasks: &mut [TaskRecord],
    turns: &mut [TurnRecord],
    calls: &[UsageCall],
    observations: &[RateObservation],
    limits: &[LimitBucket],
    now: DateTime<Utc>,
) -> (Vec<ModelUsage>, AttributionSummary) {
    reset_records(tasks, turns);

    let settled = tasks.iter().all(|task| {
        matches!(
            task.status,
            crate::domain::TaskStatus::Idle
                | crate::domain::TaskStatus::Completed
                | crate::domain::TaskStatus::Interrupted
                | crate::domain::TaskStatus::Failed
        )
    });
    let Some(selected) = select_window(limits, now) else {
        return (
            Vec::new(),
            AttributionSummary {
                settled,
                ..AttributionSummary::default()
            },
        );
    };

    let window_calls: Vec<(usize, &UsageCall)> = calls
        .iter()
        .enumerate()
        .filter(|(_, call)| {
            call.timestamp >= selected.starts_at
                && call.timestamp <= now
                && call.timestamp <= selected.ends_at
        })
        .collect();

    let mut local_token_usage = TokenUsage::default();
    for (_, call) in &window_calls {
        local_token_usage.add_assign(call.tokens);
    }

    let total_tokens = local_token_usage.total_tokens;
    let mut task_tokens: BTreeMap<String, TokenUsage> = BTreeMap::new();
    let mut turn_tokens: BTreeMap<(String, String), TokenUsage> = BTreeMap::new();
    let mut model_tokens: BTreeMap<String, TokenUsage> = BTreeMap::new();

    for (_, call) in &window_calls {
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

    for task in tasks.iter_mut() {
        task.window_token_usage = task_tokens
            .get(&task.thread_id)
            .copied()
            .unwrap_or_default();
        task.local_token_share_percent = token_share(task.window_token_usage, total_tokens);
    }
    for turn in turns.iter_mut() {
        turn.window_token_usage = turn_tokens
            .get(&(turn.thread_id.clone(), turn.turn_id.clone()))
            .copied()
            .unwrap_or_default();
        turn.local_token_share_percent = token_share(turn.window_token_usage, total_tokens);
    }

    let mut estimated_by_call = vec![0.0; calls.len()];
    let mut samples = compatible_samples(observations, &selected, now);
    add_current_sample(&mut samples, &selected, now);
    samples.sort_by(|left, right| {
        left.timestamp
            .cmp(&right.timestamp)
            .then_with(|| sample_rank(left).cmp(&sample_rank(right)))
            .then_with(|| left.is_current.cmp(&right.is_current))
    });
    samples.dedup_by(|later, earlier| {
        if later.timestamp == earlier.timestamp {
            if sample_rank(later) >= sample_rank(earlier) {
                *earlier = *later;
            }
            true
        } else {
            false
        }
    });

    // A backwards move starts a new observation epoch. Older quota deltas no
    // longer reconcile with the current server value, but later monotonic
    // snapshots can still support a low-confidence real-time estimate.
    let epoch_start = samples
        .windows(2)
        .rposition(|pair| pair[1].used_percent + PERCENT_EPSILON < pair[0].used_percent)
        .map(|index| index + 1)
        .unwrap_or(0);
    let quota_discontinuity = epoch_start > 0;
    let estimation_samples = &samples[epoch_start..];

    let current_used_percent = selected.window.used_percent.max(0.0);
    let mut observed_delta_percent = 0.0;
    let mut estimated_assigned_percent = 0.0;

    for pair in estimation_samples.windows(2) {
        let previous = pair[0];
        let current = pair[1];
        let observed_delta = (current.used_percent - previous.used_percent).max(0.0);
        if observed_delta == 0.0 {
            continue;
        }
        observed_delta_percent += observed_delta;

        if current
            .timestamp
            .signed_duration_since(previous.timestamp)
            .num_seconds()
            > MAX_ATTRIBUTION_GAP_SECS
        {
            continue;
        }

        let assignable_delta =
            observed_delta.min((current_used_percent - estimated_assigned_percent).max(0.0));
        if assignable_delta == 0.0 {
            continue;
        }

        let interval_calls: Vec<(usize, &UsageCall)> = window_calls
            .iter()
            .copied()
            .filter(|(_, call)| {
                call.timestamp > previous.timestamp && call.timestamp <= current.timestamp
            })
            .collect();
        let interval_tokens = interval_calls.iter().fold(0_u64, |total, (_, call)| {
            total.saturating_add(call.tokens.total_tokens)
        });
        if interval_tokens == 0 {
            continue;
        }

        for (index, call) in interval_calls {
            let share = call.tokens.total_tokens as f64 / interval_tokens as f64;
            estimated_by_call[index] += assignable_delta * share;
        }
        estimated_assigned_percent += assignable_delta;
    }

    let confidence = if estimated_assigned_percent == 0.0 {
        Confidence::Unknown
    } else if settled && !quota_discontinuity {
        Confidence::Medium
    } else {
        Confidence::Low
    };

    let mut task_estimates: BTreeMap<String, f64> = BTreeMap::new();
    let mut turn_estimates: BTreeMap<(String, String), f64> = BTreeMap::new();
    let mut model_estimates: BTreeMap<String, f64> = BTreeMap::new();
    for (index, call) in calls.iter().enumerate() {
        let estimate = estimated_by_call[index];
        if estimate == 0.0 {
            continue;
        }
        *task_estimates.entry(call.thread_id.clone()).or_default() += estimate;
        if let Some(turn_id) = &call.turn_id {
            *turn_estimates
                .entry((call.thread_id.clone(), turn_id.clone()))
                .or_default() += estimate;
        }
        *model_estimates.entry(model_name(call)).or_default() += estimate;
    }

    for task in tasks.iter_mut() {
        task.estimated_quota_percent = task_estimates
            .get(&task.thread_id)
            .copied()
            .unwrap_or_default();
        task.quota_confidence = if task.estimated_quota_percent > 0.0 {
            confidence
        } else {
            Confidence::Unknown
        };
    }
    for turn in turns.iter_mut() {
        turn.estimated_quota_percent = turn_estimates
            .get(&(turn.thread_id.clone(), turn.turn_id.clone()))
            .copied()
            .unwrap_or_default();
        turn.quota_confidence = if turn.estimated_quota_percent > 0.0 {
            confidence
        } else {
            Confidence::Unknown
        };
    }

    let models = model_tokens
        .into_iter()
        .map(|(model, token_usage)| ModelUsage {
            local_token_share_percent: token_share(token_usage, total_tokens),
            estimated_quota_percent: model_estimates.get(&model).copied().unwrap_or_default(),
            quota_confidence: if model_estimates.get(&model).copied().unwrap_or_default() > 0.0 {
                confidence
            } else {
                Confidence::Unknown
            },
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
            used_percent: selected.window.used_percent,
        }),
        local_token_usage,
        observed_delta_percent,
        estimated_assigned_percent,
        unattributed_percent: (current_used_percent - estimated_assigned_percent).max(0.0),
        attribution_coverage_percent: if current_used_percent == 0.0 {
            0.0
        } else {
            estimated_assigned_percent / current_used_percent * 100.0
        },
        external_activity_possible: true,
        confidence,
        method: if quota_discontinuity && estimated_assigned_percent > 0.0 {
            "post_discontinuity_observed_delta_token_proportional".to_string()
        } else if quota_discontinuity {
            "quota_discontinuity_local_tokens_only".to_string()
        } else if estimated_assigned_percent > 0.0 {
            "observed_delta_token_proportional".to_string()
        } else {
            "local_tokens_only".to_string()
        },
        settled,
    };

    (models, summary)
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

fn select_window(limits: &[LimitBucket], now: DateTime<Utc>) -> Option<SelectedWindow<'_>> {
    let mut usable = Vec::new();
    for bucket in limits {
        if let Some(window) = &bucket.primary
            && let Some(selected) = usable_window(bucket, window, WindowSlot::Primary)
        {
            usable.push(selected);
        }
        if let Some(window) = &bucket.secondary
            && let Some(selected) = usable_window(bucket, window, WindowSlot::Secondary)
        {
            usable.push(selected);
        }
    }

    let is_current = |window: &&SelectedWindow<'_>| {
        now >= window.starts_at - Duration::seconds(RESET_DRIFT_SECS) && now < window.ends_at
    };

    usable
        .iter()
        .find(|window| {
            window.window.window_duration_mins == Some(FIVE_HOURS_MINS) && is_current(window)
        })
        .map(|window| SelectedWindow {
            bucket: window.bucket,
            window: window.window,
            slot: window.slot,
            starts_at: window.starts_at,
            ends_at: window.ends_at,
        })
}

fn usable_window<'a>(
    bucket: &'a LimitBucket,
    window: &'a LimitWindow,
    slot: WindowSlot,
) -> Option<SelectedWindow<'a>> {
    let duration = window.window_duration_mins.filter(|minutes| *minutes > 0)?;
    let ends_at = window.resets_at?;
    Some(SelectedWindow {
        bucket,
        window,
        slot,
        starts_at: ends_at - Duration::minutes(duration),
        ends_at,
    })
}

fn compatible_samples(
    observations: &[RateObservation],
    selected: &SelectedWindow<'_>,
    now: DateTime<Utc>,
) -> Vec<QuotaSample> {
    let candidates = observations
        .iter()
        .filter(|observation| observation.limit_id == selected.bucket.limit_id)
        .filter_map(|observation| {
            let window = match selected.slot {
                WindowSlot::Primary => observation.primary.as_ref(),
                WindowSlot::Secondary => observation.secondary.as_ref(),
            }?;
            if !same_window(window, selected.window) {
                return None;
            }
            if observation.timestamp < selected.starts_at || observation.timestamp > now {
                return None;
            }
            Some(QuotaSample {
                timestamp: observation.timestamp,
                used_percent: window.used_percent.max(0.0),
                provenance: observation.provenance,
                is_current: false,
            })
        })
        .collect::<Vec<_>>();

    if selected.bucket.provenance != Provenance::ServerSnapshot {
        return candidates;
    }

    let server_count = candidates
        .iter()
        .filter(|sample| sample.provenance == Provenance::ServerSnapshot)
        .count();
    if server_count >= 2 {
        return candidates
            .into_iter()
            .filter(|sample| sample.provenance == Provenance::ServerSnapshot)
            .collect();
    }

    let local_agrees = candidates
        .iter()
        .filter(|sample| sample.provenance != Provenance::ServerSnapshot)
        .max_by_key(|sample| sample.timestamp)
        .is_some_and(|sample| (sample.used_percent - selected.window.used_percent).abs() <= 20.0);
    candidates
        .into_iter()
        .filter(|sample| sample.provenance == Provenance::ServerSnapshot || local_agrees)
        .collect()
}

fn add_current_sample(
    samples: &mut Vec<QuotaSample>,
    selected: &SelectedWindow<'_>,
    now: DateTime<Utc>,
) {
    let timestamp = selected.bucket.as_of.min(now).max(selected.starts_at);
    samples.push(QuotaSample {
        timestamp,
        used_percent: selected.window.used_percent.max(0.0),
        provenance: selected.bucket.provenance,
        is_current: true,
    });
}

fn sample_rank(sample: &QuotaSample) -> u8 {
    if sample.is_current {
        2
    } else if sample.provenance == Provenance::ServerSnapshot {
        1
    } else {
        0
    }
}

fn same_window(candidate: &LimitWindow, selected: &LimitWindow) -> bool {
    if candidate.window_duration_mins != selected.window_duration_mins {
        return false;
    }
    match (candidate.resets_at, selected.resets_at) {
        (Some(candidate_end), Some(selected_end)) => {
            (candidate_end - selected_end).num_seconds().abs() <= RESET_DRIFT_SECS
        }
        _ => false,
    }
}

fn token_share(tokens: TokenUsage, total_tokens: u64) -> f64 {
    if total_tokens == 0 {
        0.0
    } else {
        tokens.total_tokens as f64 / total_tokens as f64 * 100.0
    }
}

fn model_name(call: &UsageCall) -> String {
    call.model.as_deref().unwrap_or("unknown").to_string()
}
