use std::cmp::Reverse;
use std::collections::BTreeMap;
use std::thread;
use std::time::Instant;

use anyhow::{Result, anyhow};
use chrono::{DateTime, Duration, Utc};

use crate::app_server::fetch_account_snapshot;
use crate::attribution::{analyze_windows, project_five_hour_analysis};
use crate::config::CollectConfig;
use crate::domain::{
    AccountSnapshot, LimitBucket, LimitWindow, Provenance, RateObservation, RolloutDataset,
    Snapshot, SourceStatus, WindowAnalysis,
};
use crate::history::{HISTORY_RETENTION_DAYS, HistoryObservation};
use crate::perf::{RefreshMetrics, RefreshStageMetrics};
use crate::rollout::{RolloutCache, RolloutCacheMetrics, RolloutCacheRefresh, scan_rollouts};

#[derive(Clone, Debug)]
pub struct CollectionResult {
    pub snapshot: Snapshot,
    pub account: AccountSnapshot,
    pub history_observation: HistoryObservation,
}

struct LocalCollection {
    dataset: RolloutDataset,
    source: Option<SourceStatus>,
    error: Option<String>,
    unchanged: bool,
    rollout_refresh: Option<RolloutCacheRefresh>,
    rollout_metrics: Option<RolloutCacheMetrics>,
}

struct AccountCollection {
    result: Result<AccountSnapshot>,
    elapsed_us: u64,
}

fn sources_run_in_parallel(scan_local: bool, refresh_account: bool, offline: bool) -> bool {
    scan_local && refresh_account && !offline
}

fn run_sources_in_parallel<Local, Account, LocalFn, AccountFn>(
    local: LocalFn,
    account: AccountFn,
) -> (Local, thread::Result<Account>)
where
    LocalFn: FnOnce() -> Local,
    AccountFn: FnOnce() -> Account + Send,
    Account: Send,
{
    thread::scope(|scope| {
        let account = scope.spawn(account);
        let local = local();
        (local, account.join())
    })
}

fn collect_local_source(
    config: &CollectConfig,
    now: DateTime<Utc>,
    scan_local: bool,
    rollout_cache: Option<&mut RolloutCache>,
    only_if_changed: bool,
) -> LocalCollection {
    let span = config.startup_trace.span("snapshot.local_scan");
    let collection = if scan_local {
        let (scan_result, rollout_refresh, rollout_metrics) = match rollout_cache {
            Some(cache) => {
                let result = if only_if_changed {
                    cache.scan_if_changed(config, now)
                } else {
                    cache.scan(config, now).map(Some)
                };
                (result, Some(cache.last_refresh()), Some(cache.metrics()))
            }
            None => (scan_rollouts(config, now).map(Some), None, None),
        };
        match scan_result {
            Ok(Some(dataset)) => {
                let truncated = dataset.stats.truncated_files;
                let unreadable = dataset.stats.unreadable_files;
                let skipped = dataset.stats.skipped_lines;
                let ambiguous_resets = dataset.stats.ambiguous_token_resets;
                let rollout_partial =
                    truncated > 0 || unreadable > 0 || skipped > 0 || ambiguous_resets > 0;
                LocalCollection {
                    source: Some(SourceStatus {
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
                    }),
                    dataset,
                    error: None,
                    unchanged: false,
                    rollout_refresh,
                    rollout_metrics,
                }
            }
            Ok(None) => LocalCollection {
                dataset: RolloutDataset::default(),
                source: None,
                error: None,
                unchanged: true,
                rollout_refresh,
                rollout_metrics,
            },
            Err(error) => LocalCollection {
                dataset: RolloutDataset::default(),
                source: Some(SourceStatus {
                    source: "rollout_jsonl".to_string(),
                    status: "error".to_string(),
                    as_of: now,
                    message: Some(error.to_string()),
                }),
                error: Some(format!("rollout scan failed: {error:#}")),
                unchanged: false,
                rollout_refresh,
                rollout_metrics,
            },
        }
    } else {
        LocalCollection {
            dataset: RolloutDataset::default(),
            source: None,
            error: None,
            unchanged: false,
            rollout_refresh: None,
            rollout_metrics: None,
        }
    };
    span.finish_with(|| {
        format!(
            "status={} files={} lines={} tasks={} turns={}",
            collection
                .source
                .as_ref()
                .map_or("skipped", |source| source.status.as_str()),
            collection.dataset.stats.scanned_files,
            collection.dataset.stats.parsed_lines,
            collection.dataset.tasks.len(),
            collection.dataset.turns.len()
        )
    });
    collection
}

fn fetch_account_source(config: &CollectConfig) -> AccountCollection {
    let perf_started = config.perf_log.is_enabled().then(Instant::now);
    let span = config.startup_trace.span("snapshot.account_fetch");
    let result = fetch_account_snapshot(config);
    span.finish_with(|| match &result {
        Ok(account) => format!(
            "status={} refresh=true parallel=true buckets={} reset_credits={} usage={}",
            if account.errors.is_empty() && account.warnings.is_empty() {
                "ok"
            } else {
                "partial"
            },
            account.limits.len(),
            account.rate_limit_reset_credits.is_some(),
            account.usage.is_some()
        ),
        Err(_) => {
            "status=error refresh=true parallel=true buckets=0 reset_credits=false usage=false"
                .to_string()
        }
    });
    AccountCollection {
        result,
        elapsed_us: elapsed_us(perf_started),
    }
}

fn flatten_account_thread(result: thread::Result<AccountCollection>) -> AccountCollection {
    result.unwrap_or_else(|_| AccountCollection {
        result: Err(anyhow!("app-server collector thread panicked")),
        elapsed_us: 0,
    })
}

pub fn collect_snapshot(
    config: &CollectConfig,
    cached_account: Option<AccountSnapshot>,
    refresh_account: bool,
) -> CollectionResult {
    collect_snapshot_with_local(config, cached_account, refresh_account, true, None, false)
        .expect("a forced snapshot collection always returns a result")
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
        false,
    )
    .expect("a forced cached snapshot collection always returns a result")
}

/// Performs the lightweight cached discovery pass and returns `None` when no
/// rollout, title, discovery, or task-freshness state requires a new snapshot.
pub fn collect_snapshot_cached_if_changed(
    config: &CollectConfig,
    cached_account: Option<AccountSnapshot>,
    rollout_cache: &mut RolloutCache,
) -> Option<CollectionResult> {
    collect_snapshot_with_local(
        config,
        cached_account,
        false,
        true,
        Some(rollout_cache),
        true,
    )
}

pub fn collect_limits_snapshot(
    config: &CollectConfig,
    cached_account: Option<AccountSnapshot>,
    refresh_account: bool,
) -> CollectionResult {
    collect_snapshot_with_local(config, cached_account, refresh_account, false, None, false)
        .expect("a forced limits collection always returns a result")
}

fn collect_snapshot_with_local(
    config: &CollectConfig,
    cached_account: Option<AccountSnapshot>,
    refresh_account: bool,
    scan_local: bool,
    rollout_cache: Option<&mut RolloutCache>,
    only_if_changed: bool,
) -> Option<CollectionResult> {
    debug_assert!(!only_if_changed || !refresh_account);
    let total_started = config.startup_trace.is_active().then(Instant::now);
    let perf_started = config.perf_log.is_enabled().then(Instant::now);
    let now = Utc::now();
    let mut sources = Vec::new();
    let mut warnings = Vec::new();
    let mut errors = Vec::new();

    let parallel_sources = sources_run_in_parallel(scan_local, refresh_account, config.offline);
    let (local, prefetched_account, parallel_account_us) = if parallel_sources {
        let (local, account) = run_sources_in_parallel(
            || collect_local_source(config, now, scan_local, rollout_cache, only_if_changed),
            || fetch_account_source(config),
        );
        let account = flatten_account_thread(account);
        (local, Some(account.result), account.elapsed_us)
    } else {
        (
            collect_local_source(config, now, scan_local, rollout_cache, only_if_changed),
            None,
            0,
        )
    };
    if local.unchanged {
        record_refresh_performance(
            config,
            perf_started,
            false,
            false,
            local.rollout_refresh,
            local.rollout_metrics,
            0,
            0,
            0,
            RefreshStageMetrics::default(),
        );
        return None;
    }
    let rollout_refresh = local.rollout_refresh;
    let rollout_metrics = local.rollout_metrics;
    if let Some(source) = local.source {
        sources.push(source);
    }
    if let Some(error) = local.error {
        errors.push(error);
    }
    let mut dataset = local.dataset;
    warnings.append(&mut dataset.warnings);

    let account_perf_started =
        (!parallel_sources && config.perf_log.is_enabled()).then(Instant::now);
    let account_span =
        (!parallel_sources).then(|| config.startup_trace.span("snapshot.account_fetch"));
    let mut account = cached_account.unwrap_or_default();
    let previous_account_observations = account.rate_observations.clone();
    let account_status;
    if config.offline {
        account_status = "offline";
        sources.push(SourceStatus {
            source: "app_server".to_string(),
            status: "offline".to_string(),
            as_of: now,
            message: Some("disabled by --offline".to_string()),
        });
    } else if refresh_account {
        let refresh_result = prefetched_account.unwrap_or_else(|| fetch_account_snapshot(config));
        match refresh_result {
            Ok(mut fresh) => {
                account_status = if fresh.errors.is_empty() && fresh.warnings.is_empty() {
                    "ok"
                } else {
                    "partial"
                };
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
                account_status = "error";
                let warning = format!("app-server refresh failed: {error:#}");
                if !account.warnings.contains(&warning) {
                    account.warnings.push(warning);
                }
                mark_account_data_stale(&mut account);
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
        account_status = "cached";
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
    if let Some(account_span) = account_span {
        account_span.finish_with(|| {
            format!(
                "status={account_status} refresh={refresh_account} parallel=false buckets={} reset_credits={} usage={}",
                account.limits.len(),
                account.rate_limit_reset_credits.is_some(),
                account.usage.is_some()
            )
        });
    }
    let account_us = if parallel_sources {
        parallel_account_us
    } else {
        elapsed_us(account_perf_started)
    };

    let derive_perf_started = config.perf_log.is_enabled().then(Instant::now);
    let derive_span = config.startup_trace.span("snapshot.derive");
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
    let calls_count = dataset.calls.len();
    let analysis_perf_started = config.perf_log.is_enabled().then(Instant::now);
    let analysis_span = config.startup_trace.span("snapshot.window_analysis");
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
    analysis_span.finish_with(|| {
        format!(
            "windows={} models={} tasks={} turns={}",
            window_analyses.len(),
            models.len(),
            tasks.len(),
            turns.len()
        )
    });
    let window_analysis_us = elapsed_us(analysis_perf_started);

    let sort_perf_started = config.perf_log.is_enabled().then(Instant::now);
    let sort_span = config.startup_trace.span("snapshot.sort");
    tasks.sort_by_key(|task| Reverse(task.updated_at));
    turns.sort_by_key(|turn| Reverse(turn.started_at));
    sort_span.finish_with(|| format!("tasks={} turns={}", tasks.len(), turns.len()));
    let sort_us = elapsed_us(sort_perf_started);

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

    let mut history_partial_reasons = Vec::new();
    if !scan_local {
        history_partial_reasons.push("local_scan_disabled".to_string());
    } else if !rollout_complete || rollout_source_degraded {
        history_partial_reasons.push("rollout_scan_incomplete".to_string());
    }
    for reason in window_analyses
        .iter()
        .flat_map(|analysis| analysis.partial_reasons.iter())
        .filter(|reason| reason.starts_with("rollout_") || reason.as_str() == "local_scan_disabled")
    {
        if !history_partial_reasons.contains(reason) {
            history_partial_reasons.push(reason.clone());
        }
    }
    let local_coverage_starts_at = (scan_local && rollout_complete && !rollout_source_degraded)
        .then(|| now - Duration::days(config.lookback_days.clamp(1, HISTORY_RETENTION_DAYS)));
    let history_observation = HistoryObservation::from_sources_with_coverage(
        now,
        &dataset.calls,
        &limits,
        &history_partial_reasons,
        local_coverage_starts_at,
    );

    let result = CollectionResult {
        snapshot: Snapshot {
            schema_version: 1,
            as_of: now,
            partial,
            codex_home: config.codex_home.clone(),
            sources,
            limits,
            rate_limit_reset_credits: account.rate_limit_reset_credits.clone(),
            rate_limit_reset_credits_partial: account.rate_limit_reset_credits_partial,
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
        history_observation,
    };
    derive_span.finish_with(|| {
        format!(
            "partial={} limits={} windows={} warnings={} errors={}",
            result.snapshot.partial,
            result.snapshot.limits.len(),
            result.snapshot.window_analyses.len(),
            result.snapshot.warnings.len(),
            result.snapshot.errors.len()
        )
    });
    let snapshot_derive_us = elapsed_us(derive_perf_started);
    record_refresh_performance(
        config,
        perf_started,
        true,
        refresh_account,
        rollout_refresh,
        rollout_metrics,
        result.snapshot.tasks.len(),
        result.snapshot.turns.len(),
        calls_count,
        RefreshStageMetrics {
            account_us,
            snapshot_derive_us,
            window_analysis_us,
            sort_us,
            ..RefreshStageMetrics::default()
        },
    );
    if let Some(total_started) = total_started {
        config
            .startup_trace
            .record_with("snapshot.total", total_started, || {
                format!(
                    "local={scan_local} refresh_account={refresh_account} parallel_sources={parallel_sources} tasks={} turns={}",
                    result.snapshot.tasks.len(),
                    result.snapshot.turns.len()
                )
            });
    }
    Some(result)
}

#[allow(clippy::too_many_arguments)]
fn record_refresh_performance(
    config: &CollectConfig,
    started: Option<Instant>,
    changed: bool,
    account_refreshed: bool,
    rollout_refresh: Option<RolloutCacheRefresh>,
    rollout_metrics: Option<RolloutCacheMetrics>,
    tasks: usize,
    turns: usize,
    calls: usize,
    mut stages: RefreshStageMetrics,
) {
    let Some(started) = started else {
        return;
    };
    let refresh = rollout_refresh.unwrap_or_default();
    let cache = rollout_metrics.unwrap_or_default();
    stages.discover_us = refresh.discover_us;
    stages.cache_load_us = refresh.cache_load_us;
    stages.parse_us = refresh.parse_us;
    stages.cache_save_us = refresh.cache_save_us;
    stages.reduce_us = refresh.reduce_us;
    stages.materialize_us = refresh.materialize_us;
    config.perf_log.record_refresh(RefreshMetrics {
        duration_us: elapsed_us(Some(started)),
        account_refreshed,
        changed,
        reduced_rebuilt: refresh.rebuilt,
        discovery_full_scan: refresh.discovery_full_scan,
        discovery_cache_hit: refresh.discovery_cache_hit,
        discovery_invalidated: refresh.discovery_invalidated,
        discovery_probed_files: saturating_u64(refresh.discovery_probed_files),
        discovery_probed_dirs: saturating_u64(refresh.discovery_probed_dirs),
        selected_files: saturating_u64(cache.selected_files),
        selected_bytes: cache.selected_bytes,
        parsed_lines: saturating_u64(cache.parsed_lines),
        cached_events: saturating_u64(cache.cached_events),
        foreign_baseline_events: saturating_u64(cache.foreign_baseline_events),
        reparsed_files: saturating_u64(refresh.reparsed_files),
        tail_parsed_files: saturating_u64(refresh.tail_parsed_files),
        tail_parsed_bytes: refresh.tail_parsed_bytes,
        full_parsed_files: saturating_u64(refresh.full_parsed_files),
        reused_files: saturating_u64(refresh.reused_files),
        incrementally_reduced_threads: saturating_u64(refresh.incrementally_reduced_threads),
        full_rebuild: refresh.full_rebuild,
        tasks: saturating_u64(tasks),
        turns: saturating_u64(turns),
        calls: saturating_u64(calls),
        stages,
    });
}

fn elapsed_us(started: Option<Instant>) -> u64 {
    started
        .map(|started| u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

fn saturating_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
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
    let limits_refresh_failed = fresh.limits.is_empty() && !fresh.errors.is_empty();
    if fresh.limits.is_empty() && !cached.limits.is_empty() {
        fresh.limits = cached.limits.clone();
        mark_limits_stale(&mut fresh.limits);
    }
    if limits_refresh_failed && fresh.rate_limit_reset_credits.is_none() {
        fresh
            .rate_limit_reset_credits
            .clone_from(&cached.rate_limit_reset_credits);
        if let Some(reset_credits) = &mut fresh.rate_limit_reset_credits {
            reset_credits.provenance = Provenance::Stale;
        }
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

fn mark_account_data_stale(account: &mut AccountSnapshot) {
    mark_limits_stale(&mut account.limits);
    if let Some(reset_credits) = &mut account.rate_limit_reset_credits {
        reset_credits.provenance = Provenance::Stale;
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
    use crate::domain::{
        AccountTokenUsage, Confidence, RateLimitResetCredit, RateLimitResetCreditsSnapshot,
        TokenUsage, UsageCall,
    };

    #[test]
    fn only_full_online_refresh_runs_sources_in_parallel() {
        assert!(sources_run_in_parallel(true, true, false));
        assert!(!sources_run_in_parallel(false, true, false));
        assert!(!sources_run_in_parallel(true, false, false));
        assert!(!sources_run_in_parallel(true, true, true));
    }

    #[test]
    fn source_collectors_overlap_with_a_bounded_handshake() {
        let (account_started_tx, account_started_rx) = std::sync::mpsc::channel();
        let (account_release_tx, account_release_rx) = std::sync::mpsc::channel();
        let timeout = std::time::Duration::from_secs(5);

        let (local_saw_account, account) = run_sources_in_parallel(
            move || {
                let saw_account = account_started_rx.recv_timeout(timeout).is_ok();
                account_release_tx.send(()).unwrap();
                saw_account
            },
            move || {
                account_started_tx.send(()).unwrap();
                account_release_rx.recv_timeout(timeout).is_ok()
            },
        );

        assert!(local_saw_account);
        assert!(account.unwrap());
    }

    #[test]
    fn account_collector_panic_becomes_a_refresh_error() {
        let account: thread::Result<AccountCollection> =
            Err(Box::new("simulated account collector panic") as Box<dyn std::any::Any + Send>);

        let account = flatten_account_thread(account);
        let error = account.result.unwrap_err();
        assert_eq!(error.to_string(), "app-server collector thread panicked");
        assert_eq!(account.elapsed_us, 0);
    }

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
            service_tier: None,
            tokens: TokenUsage {
                input_tokens: total_tokens,
                total_tokens,
                ..TokenUsage::default()
            },
        }
    }

    fn reset_credit(
        granted_at: DateTime<Utc>,
        expires_at: Option<DateTime<Utc>>,
    ) -> RateLimitResetCredit {
        RateLimitResetCredit {
            granted_at,
            expires_at,
            status: "available".to_string(),
            reset_type: "codexRateLimits".to_string(),
            title: Some("Reset Codex limits".to_string()),
            description: Some("One reset opportunity".to_string()),
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
        let cached_credit = reset_credit(now - Duration::hours(1), Some(now + Duration::days(30)));
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
            rate_limit_reset_credits: Some(RateLimitResetCreditsSnapshot {
                available_count: 3,
                credits: Some(vec![cached_credit.clone()]),
                provenance: Provenance::ServerSnapshot,
                as_of: now,
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
        assert_eq!(
            fresh.rate_limit_reset_credits.as_ref().unwrap().provenance,
            Provenance::Stale
        );
        assert_eq!(
            fresh
                .rate_limit_reset_credits
                .as_ref()
                .unwrap()
                .credits
                .as_deref(),
            Some([cached_credit.clone()].as_slice())
        );
        assert_eq!(fresh.usage.unwrap().lifetime_tokens, Some(42));

        let mut successful_without_reset_credits = AccountSnapshot {
            limits: cached.limits.clone(),
            ..AccountSnapshot::default()
        };
        preserve_cached_account_data(&mut successful_without_reset_credits, &cached);
        assert!(
            successful_without_reset_credits
                .rate_limit_reset_credits
                .is_none()
        );

        let mut successful_with_unknown_details = AccountSnapshot {
            limits: cached.limits.clone(),
            rate_limit_reset_credits: Some(RateLimitResetCreditsSnapshot {
                available_count: 2,
                credits: None,
                provenance: Provenance::ServerSnapshot,
                as_of: now + Duration::minutes(1),
            }),
            ..AccountSnapshot::default()
        };
        preserve_cached_account_data(&mut successful_with_unknown_details, &cached);
        assert!(
            successful_with_unknown_details
                .rate_limit_reset_credits
                .as_ref()
                .unwrap()
                .credits
                .is_none()
        );

        let mut successful_with_empty_details = AccountSnapshot {
            limits: cached.limits.clone(),
            rate_limit_reset_credits: Some(RateLimitResetCreditsSnapshot {
                available_count: 0,
                credits: Some(Vec::new()),
                provenance: Provenance::ServerSnapshot,
                as_of: now + Duration::minutes(2),
            }),
            ..AccountSnapshot::default()
        };
        preserve_cached_account_data(&mut successful_with_empty_details, &cached);
        assert_eq!(
            successful_with_empty_details
                .rate_limit_reset_credits
                .as_ref()
                .unwrap()
                .credits
                .as_deref(),
            Some([].as_slice())
        );

        let mut stale = cached;
        mark_account_data_stale(&mut stale);
        assert_eq!(
            stale.rate_limit_reset_credits.as_ref().unwrap().provenance,
            Provenance::Stale
        );
        assert_eq!(
            stale.rate_limit_reset_credits.unwrap().credits.as_deref(),
            Some([cached_credit].as_slice())
        );
    }

    #[test]
    fn cached_reset_credits_flow_into_the_final_snapshot() {
        let now = Utc::now();
        let cached_credit = reset_credit(now - Duration::hours(1), None);
        let cached = AccountSnapshot {
            rate_limit_reset_credits: Some(RateLimitResetCreditsSnapshot {
                available_count: 2,
                credits: Some(vec![cached_credit.clone()]),
                provenance: Provenance::ServerSnapshot,
                as_of: now,
            }),
            ..AccountSnapshot::default()
        };
        let config = CollectConfig {
            offline: true,
            ..CollectConfig::default()
        };

        let result = collect_limits_snapshot(&config, Some(cached), false);

        let reset_credits = result.snapshot.rate_limit_reset_credits.unwrap();
        assert_eq!(reset_credits.available_count, 2);
        assert_eq!(reset_credits.as_of, now);
        assert_eq!(reset_credits.credits, Some(vec![cached_credit]));
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
            usage_call(now - Duration::hours(2), "a", "a-turn", "gpt-5.6-sol", 100),
            usage_call(now - Duration::hours(1), "b", "b-turn", "gpt-5.5", 300),
        ];
        let mut analyses = analyze_windows(&[], &[], &calls, &[], &limits, now);

        assert_eq!(analyses.len(), 1);
        let analysis = &analyses[0];
        assert_close(analysis.attribution.proxy_projected_percent, 40.0);
        assert_eq!(
            analysis.attribution.method,
            "current_codex_gauge_credit_rate_weighted_proxy"
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
                "gpt-5.6-sol",
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
        assert_eq!(analysis.models[0].model, "gpt-5.6-sol");
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
