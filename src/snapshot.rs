use chrono::{DateTime, Duration, Utc};

use crate::app_server::fetch_account_snapshot;
use crate::attribution::{
    active_bucket_ids_for_duration, analyze_windows, project_five_hour_analysis,
};
use crate::config::CollectConfig;
use crate::domain::{
    AccountSnapshot, Confidence, LimitBucket, LimitWindow, Provenance, RateObservation,
    RolloutDataset, Snapshot, SourceStatus, WindowAnalysis,
};
use crate::rollout::{RolloutCache, scan_rollouts};

#[derive(Clone, Debug)]
pub struct CollectionResult {
    pub snapshot: Snapshot,
    pub account: AccountSnapshot,
}

pub fn collect_snapshot(
    config: &CollectConfig,
    cached_account: Option<AccountSnapshot>,
    refresh_account: bool,
) -> CollectionResult {
    collect_snapshot_with_local(config, cached_account, refresh_account, true, None)
}

pub fn collect_snapshot_cached(
    config: &CollectConfig,
    cached_account: Option<AccountSnapshot>,
    refresh_account: bool,
    rollout_cache: &mut RolloutCache,
) -> CollectionResult {
    collect_snapshot_with_local(
        config,
        cached_account,
        refresh_account,
        true,
        Some(rollout_cache),
    )
}

pub fn collect_limits_snapshot(
    config: &CollectConfig,
    cached_account: Option<AccountSnapshot>,
    refresh_account: bool,
) -> CollectionResult {
    collect_snapshot_with_local(config, cached_account, refresh_account, false, None)
}

fn collect_snapshot_with_local(
    config: &CollectConfig,
    cached_account: Option<AccountSnapshot>,
    refresh_account: bool,
    scan_local: bool,
    rollout_cache: Option<&mut RolloutCache>,
) -> CollectionResult {
    let now = Utc::now();
    let mut sources = Vec::new();
    let mut warnings = Vec::new();
    let mut errors = Vec::new();

    let mut dataset = if scan_local {
        let scan_result = match rollout_cache {
            Some(cache) => cache.scan(config, now),
            None => scan_rollouts(config, now),
        };
        match scan_result {
            Ok(dataset) => {
                let truncated = dataset.stats.truncated_files;
                let unreadable = dataset.stats.unreadable_files;
                let skipped = dataset.stats.skipped_lines;
                let ambiguous_resets = dataset.stats.ambiguous_token_resets;
                let rollout_partial =
                    truncated > 0 || unreadable > 0 || skipped > 0 || ambiguous_resets > 0;
                sources.push(SourceStatus {
                source: "rollout_jsonl".to_string(),
                status: if rollout_partial {
                    "partial".to_string()
                } else {
                    "ok".to_string()
                },
                as_of: now,
                message: Some(if rollout_partial {
                    format!(
                        "{} files, {truncated} truncated, {unreadable} unreadable, {skipped} lines skipped, {ambiguous_resets} ambiguous token resets",
                        dataset.stats.scanned_files
                    )
                } else {
                    format!("{} files", dataset.stats.scanned_files)
                }),
            });
                dataset
            }
            Err(error) => {
                errors.push(format!("rollout scan failed: {error:#}"));
                sources.push(SourceStatus {
                    source: "rollout_jsonl".to_string(),
                    status: "error".to_string(),
                    as_of: now,
                    message: Some(error.to_string()),
                });
                RolloutDataset::default()
            }
        }
    } else {
        RolloutDataset::default()
    };
    warnings.append(&mut dataset.warnings);

    let mut account = cached_account.unwrap_or_default();
    let previous_account_observations = account.rate_observations.clone();
    if config.offline {
        sources.push(SourceStatus {
            source: "app_server".to_string(),
            status: "offline".to_string(),
            as_of: now,
            message: Some("disabled by --offline".to_string()),
        });
    } else if refresh_account {
        match fetch_account_snapshot(config) {
            Ok(mut fresh) => {
                merge_account_observations(&mut fresh, previous_account_observations, now);
                preserve_cached_account_data(&mut fresh, &account);
                account = fresh;
                sources.push(SourceStatus {
                    source: "app_server".to_string(),
                    status: if account.errors.is_empty() && account.warnings.is_empty() {
                        "ok".to_string()
                    } else {
                        "partial".to_string()
                    },
                    as_of: now,
                    message: None,
                });
            }
            Err(error) => {
                let warning = format!("app-server refresh failed: {error:#}");
                if !account.warnings.contains(&warning) {
                    account.warnings.push(warning);
                }
                mark_limits_stale(&mut account.limits);
                sources.push(SourceStatus {
                    source: "app_server".to_string(),
                    status: if account.limits.is_empty() {
                        "error".to_string()
                    } else {
                        "stale".to_string()
                    },
                    as_of: now,
                    message: Some(error.to_string()),
                });
            }
        }
    } else {
        sources.push(SourceStatus {
            source: "app_server".to_string(),
            status: if account.limits.is_empty() {
                "stale".to_string()
            } else if account.errors.is_empty() && account.warnings.is_empty() {
                "cached".to_string()
            } else {
                "partial".to_string()
            },
            as_of: now,
            message: if account.limits.is_empty() {
                Some("no cached account snapshot".to_string())
            } else {
                None
            },
        });
    }

    warnings.extend(account.warnings.clone());
    errors.extend(account.errors.clone());

    let limits = if account.limits.is_empty() && scan_local {
        fallback_limits(&dataset, now)
    } else {
        account.limits.clone()
    };
    if quota_sources_disagree(&dataset.rate_observations, &limits) {
        warnings.push(
            "rollout quota snapshots disagree with the current app-server snapshot; attribution uses app-server history"
                .to_string(),
        );
    }

    let mut tasks = dataset.tasks;
    let mut turns = dataset.turns;
    let mut rate_observations = dataset.rate_observations;
    rate_observations.extend(account.rate_observations.iter().cloned());
    let rollout_complete = dataset.stats.truncated_files == 0
        && dataset.stats.unreadable_files == 0
        && dataset.stats.skipped_lines == 0
        && dataset.stats.ambiguous_token_resets == 0;
    let attribution_observations = if rollout_complete {
        rate_observations.as_slice()
    } else {
        &[]
    };
    let mut window_analyses = analyze_windows(
        &tasks,
        &turns,
        &dataset.calls,
        attribution_observations,
        &limits,
        now,
    );
    let rollout_source_degraded = sources.iter().any(|source| {
        source.source == "rollout_jsonl"
            && matches!(source.status.as_str(), "error" | "partial" | "stale")
    });
    if !scan_local || !rollout_complete || rollout_source_degraded {
        let reason = if scan_local {
            "rollout_scan_incomplete"
        } else {
            "local_scan_disabled"
        };
        for analysis in &mut window_analyses {
            mark_analysis_partial(analysis, reason);
        }
    }
    if scan_local {
        mark_incomplete_window_coverage(
            &mut warnings,
            &mut window_analyses,
            now,
            config.lookback_days,
        );
    }
    mark_stale_window_analyses(&mut window_analyses, &limits);
    handle_ambiguous_window_buckets(&mut warnings, &limits, &mut window_analyses, now);
    let (models, attribution) =
        project_five_hour_analysis(&mut tasks, &mut turns, &window_analyses);
    if window_analyses
        .iter()
        .any(|analysis| analysis.attribution.method.contains("discontinuity"))
    {
        warnings.push(
            "quota percentage moved backwards inside one window; attribution uses only the later monotonic epoch"
                .to_string(),
        );
    }

    tasks.sort_by(|left, right| right.updated_at.cmp(&left.updated_at));
    turns.sort_by(|left, right| right.started_at.cmp(&left.started_at));

    let partial = !errors.is_empty()
        || limits.is_empty()
        || limits
            .iter()
            .any(|bucket| matches!(bucket.provenance, Provenance::Stale | Provenance::Unknown))
        || dataset.stats.skipped_lines > 0
        || dataset.stats.truncated_files > 0
        || dataset.stats.unreadable_files > 0
        || dataset.stats.ambiguous_token_resets > 0
        || window_analyses.iter().any(|analysis| analysis.partial)
        || sources
            .iter()
            .any(|source| matches!(source.status.as_str(), "error" | "partial" | "stale"));

    CollectionResult {
        snapshot: Snapshot {
            schema_version: 1,
            as_of: now,
            partial,
            codex_home: config.codex_home.clone(),
            sources,
            limits,
            account_usage: account.usage.clone(),
            tasks,
            turns,
            models,
            attribution,
            window_analyses,
            stats: dataset.stats,
            warnings,
            errors,
        },
        account,
    }
}

fn mark_incomplete_window_coverage(
    warnings: &mut Vec<String>,
    analyses: &mut [WindowAnalysis],
    now: DateTime<Utc>,
    lookback_days: i64,
) {
    let cutoff = Duration::try_days(lookback_days.max(0))
        .and_then(|lookback| now.checked_sub_signed(lookback))
        .unwrap_or(DateTime::<Utc>::MIN_UTC);
    let mut incomplete = Vec::new();
    for analysis in analyses {
        let Some(window) = analysis.attribution.window.as_ref() else {
            continue;
        };
        if window.starts_at < cutoff {
            incomplete.push(window.label.clone());
            mark_analysis_partial(analysis, "rollout_lookback_incomplete");
        }
    }
    if incomplete.is_empty() {
        return;
    }
    warnings.push(format!(
        "rollout --days {lookback_days} starts after the {} reset-cycle boundary; local window shares cover only scanned data",
        incomplete.join(", ")
    ));
}

fn handle_ambiguous_window_buckets(
    warnings: &mut Vec<String>,
    limits: &[LimitBucket],
    analyses: &mut [WindowAnalysis],
    now: DateTime<Utc>,
) {
    for duration_mins in [300, 10_080] {
        let bucket_ids = active_bucket_ids_for_duration(limits, now, duration_mins);
        if bucket_ids.len() < 2 {
            continue;
        }
        let Some(analysis) = analyses
            .iter()
            .find(|analysis| analysis.duration_mins == duration_mins)
        else {
            continue;
        };
        let Some(selected_id) = analysis
            .attribution
            .window
            .as_ref()
            .map(|window| window.limit_id.clone())
        else {
            continue;
        };
        let analysis = analyses
            .iter_mut()
            .find(|analysis| analysis.duration_mins == duration_mins)
            .expect("analysis selected from the same slice");
        disable_ambiguous_quota_estimation(analysis);
        warnings.push(format!(
            "multiple active {duration_mins}m quota buckets ({}); local calls lack limit ids, so analysis selected {selected_id}; other buckets remain gauge-only",
            bucket_ids.join(", ")
        ));
    }
}

fn disable_ambiguous_quota_estimation(analysis: &mut WindowAnalysis) {
    let used_percent = analysis
        .attribution
        .window
        .as_ref()
        .map(|window| window.used_percent.max(0.0))
        .unwrap_or_default();
    analysis.attribution.estimated_assigned_percent = 0.0;
    analysis.attribution.unattributed_percent = used_percent;
    analysis.attribution.attribution_coverage_percent = 0.0;
    analysis.attribution.confidence = Confidence::Unknown;
    analysis.attribution.method = "ambiguous_limit_bucket_local_tokens_only".to_string();
    for thread in &mut analysis.threads {
        thread.usage.estimated_quota_percent = 0.0;
        thread.usage.quota_confidence = Confidence::Unknown;
    }
    for turn in &mut analysis.turns {
        turn.usage.estimated_quota_percent = 0.0;
        turn.usage.quota_confidence = Confidence::Unknown;
    }
    for model in &mut analysis.models {
        model.estimated_quota_percent = 0.0;
        model.quota_confidence = Confidence::Unknown;
    }
    mark_analysis_partial(analysis, "multiple_active_limit_buckets");
}

fn mark_stale_window_analyses(analyses: &mut [WindowAnalysis], limits: &[LimitBucket]) {
    for analysis in analyses {
        let Some(limit_id) = analysis
            .attribution
            .window
            .as_ref()
            .map(|window| window.limit_id.as_str())
        else {
            continue;
        };
        if limits.iter().any(|bucket| {
            bucket.limit_id == limit_id
                && matches!(bucket.provenance, Provenance::Stale | Provenance::Unknown)
        }) {
            mark_analysis_partial(analysis, "quota_window_stale");
        }
    }
}

fn mark_analysis_partial(analysis: &mut WindowAnalysis, reason: &str) {
    analysis.partial = true;
    if !analysis.partial_reasons.iter().any(|value| value == reason) {
        analysis.partial_reasons.push(reason.to_string());
    }
}

fn preserve_cached_account_data(fresh: &mut AccountSnapshot, cached: &AccountSnapshot) {
    if fresh.limits.is_empty() && !cached.limits.is_empty() {
        fresh.limits = cached.limits.clone();
        mark_limits_stale(&mut fresh.limits);
    }
    if fresh.usage.is_none() {
        fresh.usage.clone_from(&cached.usage);
    }
}

fn mark_limits_stale(limits: &mut [LimitBucket]) {
    for bucket in limits {
        bucket.provenance = Provenance::Stale;
    }
}

fn quota_sources_disagree(observations: &[RateObservation], limits: &[LimitBucket]) -> bool {
    limits
        .iter()
        .filter(|bucket| bucket.provenance == Provenance::ServerSnapshot)
        .any(|bucket| {
            let matching = observations
                .iter()
                .filter(|observation| observation.limit_id == bucket.limit_id)
                .filter(|observation| observation.provenance != Provenance::ServerSnapshot);
            [
                (
                    bucket.primary.as_ref(),
                    matching
                        .clone()
                        .filter_map(|observation| observation.primary.as_ref())
                        .next_back(),
                ),
                (
                    bucket.secondary.as_ref(),
                    matching
                        .filter_map(|observation| observation.secondary.as_ref())
                        .next_back(),
                ),
            ]
            .into_iter()
            .any(|(server, local)| {
                server.zip(local).is_some_and(|(server, local)| {
                    same_quota_window(server, local)
                        && (server.used_percent - local.used_percent).abs() > 2.0
                })
            })
        })
}

fn same_quota_window(left: &LimitWindow, right: &LimitWindow) -> bool {
    left.window_duration_mins == right.window_duration_mins
        && left
            .resets_at
            .zip(right.resets_at)
            .is_some_and(|(left, right)| (left - right).num_seconds().abs() <= 120)
}

fn merge_account_observations(
    account: &mut AccountSnapshot,
    mut previous: Vec<RateObservation>,
    now: DateTime<Utc>,
) {
    previous.extend(account.limits.iter().map(|bucket| RateObservation {
        timestamp: bucket.as_of,
        thread_id: "app-server".to_string(),
        turn_id: None,
        limit_id: bucket.limit_id.clone(),
        primary: bucket.primary.clone(),
        secondary: bucket.secondary.clone(),
        provenance: Provenance::ServerSnapshot,
    }));
    previous.retain(|observation| observation.timestamp >= now - Duration::days(8));
    previous.sort_by(|left, right| {
        left.timestamp
            .cmp(&right.timestamp)
            .then_with(|| left.limit_id.cmp(&right.limit_id))
    });
    previous.dedup_by(|left, right| {
        left.timestamp == right.timestamp && left.limit_id == right.limit_id
    });
    if previous.len() > 4_096 {
        previous.drain(..previous.len() - 4_096);
    }
    account.rate_observations = previous;
}

fn fallback_limits(dataset: &RolloutDataset, now: DateTime<Utc>) -> Vec<LimitBucket> {
    let Some(observation) = dataset
        .rate_observations
        .iter()
        .max_by_key(|observation| observation.timestamp)
    else {
        return Vec::new();
    };

    vec![LimitBucket {
        limit_id: observation.limit_id.clone(),
        limit_name: None,
        plan_type: None,
        primary: observation.primary.clone(),
        secondary: observation.secondary.clone(),
        credits: None,
        rate_limit_reached_type: None,
        provenance: Provenance::Stale,
        as_of: observation.timestamp.min(now),
    }]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{AccountTokenUsage, ThreadWindowUsage, TokenUsage, WindowUsage};

    #[test]
    fn failed_fresh_fields_preserve_cached_data_as_stale() {
        let now = Utc::now();
        let cached = AccountSnapshot {
            limits: vec![LimitBucket {
                limit_id: "codex".to_string(),
                limit_name: None,
                plan_type: Some("pro".to_string()),
                primary: Some(LimitWindow::new(
                    40.0,
                    Some(300),
                    Some(now + Duration::hours(2)),
                )),
                secondary: None,
                credits: None,
                rate_limit_reached_type: None,
                provenance: Provenance::ServerSnapshot,
                as_of: now,
            }],
            usage: Some(AccountTokenUsage {
                lifetime_tokens: Some(42),
                ..AccountTokenUsage::default()
            }),
            ..AccountSnapshot::default()
        };
        let mut fresh = AccountSnapshot {
            errors: vec!["rate limit unavailable".to_string()],
            ..AccountSnapshot::default()
        };

        preserve_cached_account_data(&mut fresh, &cached);

        assert_eq!(fresh.limits.len(), 1);
        assert_eq!(fresh.limits[0].provenance, Provenance::Stale);
        assert_eq!(fresh.usage.unwrap().lifetime_tokens, Some(42));
    }

    #[test]
    fn warns_when_window_analysis_must_choose_between_active_buckets() {
        let now = Utc::now();
        let reset = now + Duration::days(2);
        let make_limit = |limit_id: &str, provenance| LimitBucket {
            limit_id: limit_id.to_string(),
            limit_name: None,
            plan_type: None,
            primary: None,
            secondary: Some(LimitWindow::new(20.0, Some(10_080), Some(reset))),
            credits: None,
            rate_limit_reached_type: None,
            provenance,
            as_of: now,
        };
        let limits = vec![
            make_limit("codex-secondary", Provenance::ServerSnapshot),
            make_limit("codex", Provenance::Stale),
        ];
        let mut analyses = analyze_windows(&[], &[], &[], &[], &limits, now);
        analyses[0].attribution.estimated_assigned_percent = 2.0;
        analyses[0].attribution.confidence = Confidence::Medium;
        analyses[0].threads.push(ThreadWindowUsage {
            thread_id: "thread".to_string(),
            usage: WindowUsage {
                token_usage: TokenUsage {
                    total_tokens: 100,
                    ..TokenUsage::default()
                },
                local_token_share_percent: 100.0,
                estimated_quota_percent: 2.0,
                quota_confidence: Confidence::Medium,
            },
        });
        let mut warnings = Vec::new();

        handle_ambiguous_window_buckets(&mut warnings, &limits, &mut analyses, now);

        assert_eq!(analyses.len(), 1);
        assert_eq!(
            analyses[0].attribution.window.as_ref().unwrap().limit_id,
            "codex"
        );
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("analysis selected codex"));
        assert!(warnings[0].contains("other buckets remain gauge-only"));
        assert!(analyses[0].partial);
        assert!(
            analyses[0]
                .partial_reasons
                .contains(&"multiple_active_limit_buckets".to_string())
        );
        assert_eq!(analyses[0].attribution.estimated_assigned_percent, 0.0);
        assert_eq!(analyses[0].attribution.confidence, Confidence::Unknown);
        assert_eq!(analyses[0].threads[0].usage.estimated_quota_percent, 0.0);
        assert_eq!(
            analyses[0].threads[0].usage.quota_confidence,
            Confidence::Unknown
        );
    }

    #[test]
    fn short_rollout_lookback_marks_weekly_cycle_coverage_incomplete() {
        let now = Utc::now();
        let limits = vec![LimitBucket {
            limit_id: "codex".to_string(),
            limit_name: None,
            plan_type: None,
            primary: None,
            secondary: Some(LimitWindow::new(
                20.0,
                Some(10_080),
                Some(now + Duration::days(2)),
            )),
            credits: None,
            rate_limit_reached_type: None,
            provenance: Provenance::ServerSnapshot,
            as_of: now,
        }];
        let analyses = analyze_windows(&[], &[], &[], &[], &limits, now);
        let mut short = analyses.clone();
        let mut warnings = Vec::new();

        mark_incomplete_window_coverage(&mut warnings, &mut short, now, 1);
        assert!(warnings[0].contains("week reset-cycle boundary"));
        assert!(short[0].partial);
        assert!(
            short[0]
                .partial_reasons
                .contains(&"rollout_lookback_incomplete".to_string())
        );

        let mut complete = analyses.clone();
        warnings.clear();
        mark_incomplete_window_coverage(&mut warnings, &mut complete, now, 7);
        mark_incomplete_window_coverage(&mut warnings, &mut complete, now, i64::MAX);
        assert!(!complete[0].partial);
        assert!(warnings.is_empty());
    }
}
