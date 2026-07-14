use std::collections::BTreeMap;

use chrono::{DateTime, Duration, Utc};

use crate::app_server::fetch_account_snapshot;
use crate::attribution::{analyze_windows, project_five_hour_analysis};
use crate::config::CollectConfig;
use crate::domain::{
    AccountSnapshot, LimitBucket, LimitWindow, Provenance, RateObservation, RolloutDataset,
    Snapshot, SourceStatus, WindowAnalysis,
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
            "rollout quota snapshots disagree with the selected quota snapshot; estimates use the selected current codex gauge"
                .to_string(),
        );
    }

    let mut tasks = dataset.tasks;
    let mut turns = dataset.turns;
    let rollout_complete = dataset.stats.truncated_files == 0
        && dataset.stats.unreadable_files == 0
        && dataset.stats.skipped_lines == 0
        && dataset.stats.ambiguous_token_resets == 0;
    let mut window_analyses = analyze_windows(&tasks, &turns, &dataset.calls, &[], &limits, now);
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
    let (models, attribution) =
        project_five_hour_analysis(&mut tasks, &mut turns, &window_analyses);

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
    let mut latest_by_bucket: BTreeMap<String, &RateObservation> = BTreeMap::new();
    for observation in &dataset.rate_observations {
        let key = observation.limit_id.trim().to_ascii_lowercase();
        let replace = latest_by_bucket
            .get(&key)
            .is_none_or(|current| observation.timestamp > current.timestamp);
        if replace {
            latest_by_bucket.insert(key, observation);
        }
    }

    latest_by_bucket
        .into_values()
        .map(|observation| LimitBucket {
            limit_id: observation.limit_id.clone(),
            limit_name: None,
            plan_type: None,
            primary: observation.primary.clone(),
            secondary: observation.secondary.clone(),
            credits: None,
            rate_limit_reached_type: None,
            provenance: Provenance::Stale,
            as_of: observation.timestamp.min(now),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{AccountTokenUsage, Confidence, TokenUsage, UsageCall};

    fn weekly_limit(
        now: DateTime<Utc>,
        reset: DateTime<Utc>,
        limit_id: &str,
        used_percent: f64,
    ) -> LimitBucket {
        LimitBucket {
            limit_id: limit_id.to_string(),
            limit_name: None,
            plan_type: None,
            primary: None,
            secondary: Some(LimitWindow::new(used_percent, Some(10_080), Some(reset))),
            credits: None,
            rate_limit_reached_type: None,
            provenance: Provenance::ServerSnapshot,
            as_of: now,
        }
    }

    fn usage_call(
        timestamp: DateTime<Utc>,
        thread_id: &str,
        turn_id: &str,
        model: &str,
        total_tokens: u64,
    ) -> UsageCall {
        UsageCall {
            timestamp,
            thread_id: thread_id.to_string(),
            turn_id: Some(turn_id.to_string()),
            model: Some(model.to_string()),
            tokens: TokenUsage {
                input_tokens: total_tokens,
                total_tokens,
                ..TokenUsage::default()
            },
        }
    }

    fn assert_close(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() < 1e-9,
            "expected {expected}, got {actual}"
        );
    }

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
    fn rollout_fallback_keeps_the_latest_observation_for_each_bucket() {
        let now = Utc::now();
        let observation = |limit_id: &str, minutes_ago: i64, used_percent: f64| RateObservation {
            timestamp: now - Duration::minutes(minutes_ago),
            thread_id: "thread".to_string(),
            turn_id: None,
            limit_id: limit_id.to_string(),
            primary: Some(LimitWindow::new(
                used_percent,
                Some(300),
                Some(now + Duration::hours(2)),
            )),
            secondary: None,
            provenance: Provenance::LocalExact,
        };
        let dataset = RolloutDataset {
            rate_observations: vec![
                observation("codex", 10, 10.0),
                observation("codex_bengalfox", 5, 30.0),
                observation("CODEX", 1, 20.0),
            ],
            ..RolloutDataset::default()
        };

        let limits = fallback_limits(&dataset, now);

        assert_eq!(limits.len(), 2);
        let codex = limits
            .iter()
            .find(|bucket| bucket.limit_id.eq_ignore_ascii_case("codex"))
            .unwrap();
        let spark = limits
            .iter()
            .find(|bucket| bucket.limit_id == "codex_bengalfox")
            .unwrap();
        assert_eq!(codex.primary.as_ref().unwrap().used_percent, 20.0);
        assert_eq!(spark.primary.as_ref().unwrap().used_percent, 30.0);
        assert!(
            limits
                .iter()
                .all(|bucket| bucket.provenance == Provenance::Stale)
        );
    }

    #[test]
    fn codex_gauge_estimate_is_immediate_and_survives_partial_markers() {
        let now = Utc::now();
        let reset = now + Duration::days(2);
        let limits = vec![weekly_limit(now, reset, "codex", 40.0)];
        let calls = vec![
            usage_call(now - Duration::hours(2), "a", "a-turn", "gpt-a", 100),
            usage_call(now - Duration::hours(1), "b", "b-turn", "gpt-b", 300),
        ];
        let mut analyses = analyze_windows(&[], &[], &calls, &[], &limits, now);

        assert_eq!(analyses.len(), 1);
        let analysis = &analyses[0];
        assert_close(analysis.attribution.proxy_projected_percent, 40.0);
        assert_eq!(
            analysis.attribution.method,
            "current_codex_gauge_token_share_proxy"
        );
        assert_eq!(analysis.attribution.confidence, Confidence::Low);
        assert_close(analysis.threads[0].usage.estimated_quota_percent, 10.0);
        assert_close(analysis.threads[1].usage.estimated_quota_percent, 30.0);

        let mut stale_limits = limits;
        stale_limits[0].provenance = Provenance::Stale;
        analyses = analyze_windows(&[], &[], &calls, &[], &stale_limits, now);
        mark_analysis_partial(&mut analyses[0], "rollout_lookback_incomplete");

        let analysis = &analyses[0];
        assert!(analysis.partial);
        assert!(
            analysis
                .partial_reasons
                .contains(&"quota_window_stale".to_string())
        );
        assert!(
            analysis
                .partial_reasons
                .contains(&"rollout_lookback_incomplete".to_string())
        );
        assert_close(analysis.threads[0].usage.estimated_quota_percent, 10.0);
        assert_close(analysis.threads[1].usage.estimated_quota_percent, 30.0);
        assert_eq!(analysis.attribution.confidence, Confidence::Low);
    }

    #[test]
    fn spark_calls_and_bengalfox_bucket_do_not_enter_codex_estimates() {
        let now = Utc::now();
        let reset = now + Duration::days(2);
        let limits = vec![
            weekly_limit(now, reset, "codex", 34.0),
            weekly_limit(now, reset, "codex_bengalfox", 20.0),
        ];
        let calls = vec![
            usage_call(
                now - Duration::minutes(2),
                "regular",
                "regular-turn",
                "gpt-5.6-codex",
                300,
            ),
            usage_call(
                now - Duration::minutes(1),
                "spark",
                "spark-turn",
                "gpt-5.3-codex-spark",
                100,
            ),
        ];

        let analyses = analyze_windows(&[], &[], &calls, &[], &limits, now);

        assert_eq!(analyses.len(), 1);
        let analysis = &analyses[0];
        assert_eq!(
            analysis.attribution.window.as_ref().unwrap().limit_id,
            "codex"
        );
        assert_eq!(analysis.attribution.local_token_usage.total_tokens, 300);
        assert_eq!(analysis.threads.len(), 1);
        assert_eq!(analysis.threads[0].thread_id, "regular");
        assert_eq!(analysis.models.len(), 1);
        assert_eq!(analysis.models[0].model, "gpt-5.6-codex");
        assert_close(analysis.models[0].estimated_quota_percent, 34.0);
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
