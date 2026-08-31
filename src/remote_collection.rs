//! Rollout-only collection boundary for the short-lived remote exporter.
//!
//! Remote collection must not inherit the normal snapshot collector's account
//! and App Server behavior. This adapter validates the requested export range,
//! scans the fixed 35-day journal domain through [`RolloutCache`], and returns
//! both the source records and a guarded lower-bound history observation.

use std::collections::BTreeMap;
use std::num::NonZeroU64;

use anyhow::{Result, bail};
use chrono::{DateTime, Duration, Utc};

use crate::config::CollectConfig;
use crate::domain::{CollectionStats, RolloutDataset};
use crate::history::HistoryObservation;
use crate::remote_export_state::RemoteExportReconcileMode;
use crate::remote_protocol::{ExportRange, RemoteDeltaStats, RemoteDeltaWarning};
use crate::rollout::{RolloutCache, RolloutCacheMetrics, RolloutCacheRefresh};
use crate::source_history::RedactionProfile;

/// The one and only aggregate journal domain used by the remote exporter.
///
/// A 7d/30d UI query must never narrow this scan. The journal has one global
/// cursor, so allowing a request range to control discovery would make a
/// narrow request permanently skip records when that cursor is later reused.
pub const REMOTE_COLLECTION_MAX_LOOKBACK_DAYS: i64 = 35;
/// A remote source may have substantially more sessions than an interactive
/// local view.  Keep the exporter floor high enough to avoid silent truncation
/// under the normal local default of 500 files.
pub const REMOTE_COLLECTION_MIN_MAX_FILES: usize = 5_000;

const PARTIAL_SCAN_INCOMPLETE: &str = "rollout_scan_incomplete";
const PARTIAL_DISCOVERY_INCOMPLETE: &str = "rollout_discovery_incomplete";
const PARTIAL_FILES_TRUNCATED: &str = "rollout_files_truncated";
const PARTIAL_FILES_UNREADABLE: &str = "rollout_files_unreadable";
const PARTIAL_LINES_SKIPPED: &str = "rollout_lines_skipped";
const PARTIAL_AMBIGUOUS_RESETS: &str = "rollout_token_resets_ambiguous";

/// One source-local scan ready for normalization and journal reconciliation.
///
/// `history_observation` deliberately has no continuous coverage start.  A
/// one-shot remote scan proves only the events it found; it cannot prove that
/// the time before the exporter started observing contained zero usage.
#[derive(Clone, Debug)]
pub struct RemoteCollection {
    pub dataset: RolloutDataset,
    pub scan_complete: bool,
    /// Stable machine codes suitable for history buckets and wire coverage.
    pub partial_reasons: Vec<String>,
    /// Raw collection counters. Journal and emitted-page fields remain zero so
    /// the response builder can add its own page-specific counts.
    pub delta_stats: RemoteDeltaStats,
    /// Bounded, content-free warning categories for a remote response.
    pub delta_warnings: Vec<RemoteDeltaWarning>,
    pub cache_refresh: RolloutCacheRefresh,
    pub cache_metrics: RolloutCacheMetrics,
    history_observation: HistoryObservation,
}

/// A complete aggregate candidate that is safe to compare with the durable
/// materialized set.
///
/// This is deliberately upsert-only: even a complete one-shot inventory does
/// not yet prove that an omitted historical key should be deleted. Explicit
/// retention/coverage evidence must be added before authoritative tombstones
/// are allowed.
#[derive(Clone, Copy, Debug)]
pub struct RemoteAggregatePublication<'a> {
    observation: &'a HistoryObservation,
}

impl RemoteAggregatePublication<'_> {
    pub fn observation(&self) -> &HistoryObservation {
        self.observation
    }

    /// Absence is never authoritative in the current exporter policy.
    pub fn reconcile_mode(&self) -> RemoteExportReconcileMode {
        RemoteExportReconcileMode::UpsertOnly
    }
}

impl RemoteCollection {
    /// Returns aggregates only when the fixed-domain rollout inventory is
    /// complete.
    ///
    /// Partial scans may still be used for bounded diagnostics and a transient
    /// live view, but publishing their aggregates could replace an existing
    /// complete bucket/digest with a smaller value. Returning `None` makes the
    /// safe behavior explicit: do not plan or reconcile aggregate records, and
    /// therefore do not emit tombstones either.
    pub fn aggregate_publication(&self) -> Option<RemoteAggregatePublication<'_>> {
        self.scan_complete.then_some(RemoteAggregatePublication {
            observation: &self.history_observation,
        })
    }
}

/// Collects a remote source using a fresh in-memory cache facade.  When
/// `CollectConfig::rollout_cache_dir` is configured, `RolloutCache` hydrates
/// and updates its persistent parsed-file cache across exporter invocations.
pub fn collect_remote_rollouts(
    config: &CollectConfig,
    requested_range: &ExportRange,
    observed_at: DateTime<Utc>,
    redaction_profile: RedactionProfile,
) -> Result<RemoteCollection> {
    let mut cache = RolloutCache::new();
    collect_remote_rollouts_with_cache(
        config,
        requested_range,
        observed_at,
        redaction_profile,
        &mut cache,
    )
}

/// Cache-injectable form used by long-lived tests and callers that intentionally
/// coalesce more than one request inside the same exporter process.
pub fn collect_remote_rollouts_with_cache(
    config: &CollectConfig,
    requested_range: &ExportRange,
    observed_at: DateTime<Utc>,
    redaction_profile: RedactionProfile,
    cache: &mut RolloutCache,
) -> Result<RemoteCollection> {
    let worker_config =
        remote_worker_config(config, requested_range, observed_at, redaction_profile)?;

    // This is intentionally the only collection call in this module.  Calling
    // RolloutCache directly guarantees there is no account/App Server request,
    // even if the caller supplied paths for those collectors.
    let dataset = cache.scan(&worker_config, observed_at)?;
    let cache_refresh = cache.last_refresh();
    let cache_metrics = cache.metrics();
    let (scan_complete, partial_reasons) = collection_quality(&dataset.stats, cache_refresh);
    let warnings_suppressed = suppressed_warning_count(&dataset.warnings);
    let delta_stats = raw_delta_stats(&dataset.stats, cache_refresh, warnings_suppressed);
    let delta_warnings = remote_warnings(&dataset.stats, cache_refresh, warnings_suppressed);

    let history_observation =
        HistoryObservation::from_sources_with_tasks_turns_and_interactions_and_coverage(
            observed_at,
            &dataset.calls,
            &dataset.tasks,
            &dataset.turns,
            &dataset.agent_interactions,
            &[],
            &partial_reasons,
            // Never manufacture prospective or historical zero-usage coverage
            // from a one-shot source scan.
            None,
        );

    Ok(RemoteCollection {
        dataset,
        history_observation,
        scan_complete,
        partial_reasons,
        delta_stats,
        delta_warnings,
        cache_refresh,
        cache_metrics,
    })
}

fn remote_worker_config(
    config: &CollectConfig,
    requested_range: &ExportRange,
    _observed_at: DateTime<Utc>,
    redaction_profile: RedactionProfile,
) -> Result<CollectConfig> {
    validate_requested_range(requested_range)?;

    let mut worker = config.clone();
    worker.offline = true;
    worker.codex_bin = None;
    worker.app_server_path = None;
    worker.redact_content = redaction_profile == RedactionProfile::Redacted;
    // A global DeltaCursor is shared by every center-side reporting range.
    // Always discover the complete retention domain; the request range is
    // validated for protocol bounds but must not narrow materialization.
    worker.lookback_days = REMOTE_COLLECTION_MAX_LOOKBACK_DAYS;
    worker.max_files = worker.max_files.max(REMOTE_COLLECTION_MIN_MAX_FILES);
    Ok(worker)
}

fn validate_requested_range(range: &ExportRange) -> Result<()> {
    if range.from >= range.to {
        bail!("remote export range must have from before to");
    }
    if range.to.signed_duration_since(range.from)
        > Duration::days(REMOTE_COLLECTION_MAX_LOOKBACK_DAYS)
    {
        bail!("remote export range exceeds the 35-day retention window");
    }
    Ok(())
}

fn collection_quality(
    stats: &CollectionStats,
    refresh: RolloutCacheRefresh,
) -> (bool, Vec<String>) {
    let mut reasons = Vec::new();
    if !refresh.discovery_complete {
        reasons.push(PARTIAL_DISCOVERY_INCOMPLETE.to_owned());
    }
    if stats.truncated_files > 0 || stats.scanned_files != stats.discovered_files {
        reasons.push(PARTIAL_FILES_TRUNCATED.to_owned());
    }
    if stats.unreadable_files > 0 {
        reasons.push(PARTIAL_FILES_UNREADABLE.to_owned());
    }
    if stats.skipped_lines > 0 {
        reasons.push(PARTIAL_LINES_SKIPPED.to_owned());
    }
    if stats.ambiguous_token_resets > 0 {
        reasons.push(PARTIAL_AMBIGUOUS_RESETS.to_owned());
    }
    if !reasons.is_empty() {
        reasons.push(PARTIAL_SCAN_INCOMPLETE.to_owned());
        reasons.sort_unstable();
        reasons.dedup();
    }
    (reasons.is_empty(), reasons)
}

fn raw_delta_stats(
    stats: &CollectionStats,
    refresh: RolloutCacheRefresh,
    warnings_suppressed: u64,
) -> RemoteDeltaStats {
    let parsed_files = saturating_u64(refresh.reparsed_files);
    let reused_files = saturating_u64(refresh.reused_files);
    let unreadable_files = saturating_u64(stats.unreadable_files);
    // CollectionStats counts only successfully inventoried files as
    // discovered, while unreadable_files can also count a directory/path that
    // failed before it entered the inventory.  The wire schema defines the
    // latter as a subset of discovered files, so widen only the wire total;
    // the exact local counters remain available in `dataset.stats`.
    let discovered_files = saturating_u64(stats.discovered_files)
        .max(parsed_files)
        .max(reused_files)
        .max(unreadable_files);
    RemoteDeltaStats {
        discovered_files,
        parsed_files,
        reused_files,
        unreadable_files,
        truncated_files: saturating_u64(stats.truncated_files),
        skipped_lines: saturating_u64(stats.skipped_lines),
        ambiguous_token_resets: saturating_u64(stats.ambiguous_token_resets),
        warnings_suppressed,
        ..RemoteDeltaStats::default()
    }
}

fn remote_warnings(
    stats: &CollectionStats,
    refresh: RolloutCacheRefresh,
    warnings_suppressed: u64,
) -> Vec<RemoteDeltaWarning> {
    let mut counts = BTreeMap::<&'static str, u64>::new();
    add_warning(
        &mut counts,
        "rollout_discovery_incomplete",
        u64::from(!refresh.discovery_complete),
    );
    add_warning(
        &mut counts,
        "rollout_files_truncated",
        saturating_u64(stats.truncated_files),
    );
    add_warning(
        &mut counts,
        "rollout_files_unreadable",
        saturating_u64(stats.unreadable_files),
    );
    add_warning(
        &mut counts,
        "rollout_lines_skipped",
        saturating_u64(stats.skipped_lines),
    );
    add_warning(
        &mut counts,
        "rollout_token_resets_ambiguous",
        saturating_u64(stats.ambiguous_token_resets),
    );
    add_warning(
        &mut counts,
        "rollout_warnings_suppressed",
        warnings_suppressed,
    );
    add_warning(
        &mut counts,
        "rollout_cache_entries_corrupt",
        saturating_u64(refresh.disk_corrupt_files),
    );
    add_warning(
        &mut counts,
        "rollout_cache_write_failures",
        saturating_u64(refresh.disk_write_failures),
    );
    add_warning(
        &mut counts,
        "rollout_cache_entries_oversized",
        saturating_u64(refresh.disk_oversized_files),
    );

    counts
        .into_iter()
        .filter_map(|(code, occurrences)| {
            NonZeroU64::new(occurrences).map(|occurrences| RemoteDeltaWarning {
                code: code.to_owned(),
                occurrences,
            })
        })
        .collect()
}

fn add_warning(counts: &mut BTreeMap<&'static str, u64>, code: &'static str, occurrences: u64) {
    if occurrences > 0 {
        counts.insert(code, occurrences);
    }
}

fn suppressed_warning_count(warnings: &[String]) -> u64 {
    warnings
        .last()
        .and_then(|warning| warning.strip_prefix("suppressed "))
        .and_then(|warning| warning.strip_suffix(" additional rollout warnings"))
        .and_then(|count| count.parse::<u64>().ok())
        .unwrap_or(0)
}

fn saturating_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::Duration as StdDuration;

    use chrono::TimeZone;
    use serde_json::json;

    use super::*;

    fn at(hour: u32, minute: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 30, hour, minute, 0)
            .single()
            .unwrap()
    }

    fn range(from: DateTime<Utc>, to: DateTime<Utc>) -> ExportRange {
        ExportRange { from, to }
    }

    fn write_rollout(root: &std::path::Path, records: &[serde_json::Value]) {
        let sessions = root.join("sessions/2026/08/30");
        fs::create_dir_all(&sessions).unwrap();
        let contents = records
            .iter()
            .map(serde_json::Value::to_string)
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        fs::write(sessions.join("rollout-remote.jsonl"), contents).unwrap();
    }

    fn session_records(now: DateTime<Utc>) -> Vec<serde_json::Value> {
        vec![
            json!({
                "timestamp": (now - Duration::minutes(3)).to_rfc3339(),
                "type": "session_meta",
                "payload": {
                    "id": "01a00000-0000-7000-8000-000000000001",
                    "timestamp": (now - Duration::minutes(3)).to_rfc3339(),
                    "cwd": "/workspace/project"
                }
            }),
            json!({
                "timestamp": (now - Duration::minutes(2)).to_rfc3339(),
                "type": "event_msg",
                "payload": {"type": "task_started", "turn_id": "turn-1"}
            }),
            json!({
                "timestamp": (now - Duration::minutes(2)).to_rfc3339(),
                "type": "turn_context",
                "payload": {"turn_id": "turn-1", "model": "gpt-5.6-sol"}
            }),
            json!({
                "timestamp": (now - Duration::minutes(2)).to_rfc3339(),
                "type": "response_item",
                "payload": {
                    "type": "function_call",
                    "name": "spawn_agent",
                    "call_id": "spawn-1",
                    "internal_chat_message_metadata_passthrough": {"turn_id": "turn-1"}
                }
            }),
            json!({
                "timestamp": (now - Duration::minutes(2)).to_rfc3339(),
                "type": "event_msg",
                "payload": {
                    "type": "sub_agent_activity",
                    "kind": "started",
                    "event_id": "spawn-1",
                    "agent_thread_id": "01a00000-0000-7000-8000-000000000002"
                }
            }),
            json!({
                "timestamp": (now - Duration::minutes(1)).to_rfc3339(),
                "type": "event_msg",
                "payload": {
                    "type": "token_count",
                    "info": {"total_token_usage": {
                        "input_tokens": 80,
                        "cached_input_tokens": 40,
                        "output_tokens": 20,
                        "reasoning_output_tokens": 10,
                        "total_tokens": 100
                    }}
                }
            }),
        ]
    }

    #[test]
    fn complete_scan_returns_rollouts_and_lower_bound_history() {
        let temp = tempfile::tempdir().unwrap();
        let now = at(12, 0);
        write_rollout(temp.path(), &session_records(now));
        let config = CollectConfig {
            codex_home: temp.path().to_owned(),
            rollout_cache_dir: Some(temp.path().join("cache")),
            active_grace: StdDuration::from_secs(300),
            ..CollectConfig::default()
        };

        let collected = collect_remote_rollouts(
            &config,
            &range(now - Duration::days(1), now),
            now,
            RedactionProfile::Redacted,
        )
        .unwrap();

        assert!(collected.scan_complete);
        assert!(collected.cache_refresh.discovery_complete);
        assert!(collected.partial_reasons.is_empty());
        assert_eq!(collected.dataset.tasks.len(), 1);
        assert_eq!(collected.dataset.turns.len(), 1);
        assert_eq!(collected.dataset.agent_interactions.len(), 1);
        assert_eq!(collected.dataset.calls.len(), 1);
        assert_eq!(collected.dataset.calls[0].tokens.total_tokens, 100);
        let publication = collected.aggregate_publication().unwrap();
        assert_eq!(publication.observation().half_hour_buckets.len(), 1);
        assert_eq!(
            publication.reconcile_mode(),
            RemoteExportReconcileMode::UpsertOnly
        );
        assert_eq!(collected.delta_stats.discovered_files, 1);
        assert_eq!(collected.delta_stats.parsed_files, 1);
        assert!(collected.delta_warnings.is_empty());
    }

    #[test]
    fn truncation_and_parse_damage_are_strictly_incomplete() {
        let stats = CollectionStats {
            discovered_files: 7,
            scanned_files: 5,
            truncated_files: 2,
            unreadable_files: 1,
            skipped_lines: 3,
            ambiguous_token_resets: 1,
            ..CollectionStats::default()
        };
        let refresh = RolloutCacheRefresh {
            discovery_full_scan: true,
            discovery_complete: true,
            ..RolloutCacheRefresh::default()
        };

        let (complete, reasons) = collection_quality(&stats, refresh);

        assert!(!complete);
        assert_eq!(
            reasons,
            vec![
                "rollout_files_truncated",
                "rollout_files_unreadable",
                "rollout_lines_skipped",
                "rollout_scan_incomplete",
                "rollout_token_resets_ambiguous",
            ]
        );
        let warnings = remote_warnings(&stats, refresh, 4);
        assert_eq!(warnings.len(), 5);
        assert_eq!(warnings[0].code, "rollout_files_truncated");
        assert_eq!(warnings[0].occurrences.get(), 2);

        let empty_inventory_stats = CollectionStats {
            unreadable_files: 1,
            ..CollectionStats::default()
        };
        let wire_stats = raw_delta_stats(&empty_inventory_stats, refresh, 0);
        assert_eq!(wire_stats.discovered_files, 1);
        assert_eq!(wire_stats.unreadable_files, 1);

        let incomplete_discovery = RolloutCacheRefresh {
            discovery_full_scan: true,
            discovery_complete: false,
            ..RolloutCacheRefresh::default()
        };
        let (complete, reasons) =
            collection_quality(&CollectionStats::default(), incomplete_discovery);
        assert!(!complete);
        assert_eq!(
            reasons,
            vec!["rollout_discovery_incomplete", "rollout_scan_incomplete"]
        );
    }

    #[test]
    fn future_rollout_samples_are_not_exported_or_zero_filled() {
        let temp = tempfile::tempdir().unwrap();
        let now = at(12, 0);
        let mut records = session_records(now);
        records.push(json!({
            "timestamp": (now + Duration::hours(1)).to_rfc3339(),
            "type": "event_msg",
            "payload": {
                "type": "token_count",
                "info": {"total_token_usage": {
                    "input_tokens": 800,
                    "cached_input_tokens": 400,
                    "output_tokens": 200,
                    "reasoning_output_tokens": 100,
                    "total_tokens": 1000
                }}
            }
        }));
        write_rollout(temp.path(), &records);
        let config = CollectConfig {
            codex_home: temp.path().to_owned(),
            rollout_cache_dir: Some(temp.path().join("cache")),
            ..CollectConfig::default()
        };

        let collected = collect_remote_rollouts(
            &config,
            &range(now - Duration::hours(1), now + Duration::hours(2)),
            now,
            RedactionProfile::Redacted,
        )
        .unwrap();

        assert_eq!(collected.dataset.calls.len(), 1);
        assert_eq!(collected.dataset.calls[0].tokens.total_tokens, 100);
        let publication = collected.aggregate_publication().unwrap();
        assert_eq!(publication.observation().half_hour_buckets.len(), 1);
        assert!(
            publication
                .observation()
                .half_hour_buckets
                .iter()
                .all(|bucket| bucket.starts_at <= now)
        );
    }

    #[test]
    fn reporting_range_never_narrows_fixed_worker_domain() {
        let base = CollectConfig {
            lookback_days: 2,
            max_files: 9,
            offline: false,
            redact_content: false,
            ..CollectConfig::default()
        };
        let now = at(12, 0);
        let worker = remote_worker_config(
            &base,
            &range(
                now - Duration::days(6) - Duration::seconds(1),
                now + Duration::hours(1),
            ),
            now,
            RedactionProfile::Redacted,
        )
        .unwrap();

        assert!(worker.offline);
        assert!(worker.codex_bin.is_none());
        assert!(worker.app_server_path.is_none());
        assert!(worker.redact_content);
        assert_eq!(worker.lookback_days, REMOTE_COLLECTION_MAX_LOOKBACK_DAYS);
        assert_eq!(worker.max_files, REMOTE_COLLECTION_MIN_MAX_FILES);

        let old = remote_worker_config(
            &base,
            &range(now - Duration::days(35), now),
            now,
            RedactionProfile::PreviewEnabled,
        )
        .unwrap();
        assert!(!old.redact_content);
        assert_eq!(old.lookback_days, REMOTE_COLLECTION_MAX_LOOKBACK_DAYS);

        let prospective = remote_worker_config(
            &base,
            &range(now + Duration::hours(1), now + Duration::hours(2)),
            now,
            RedactionProfile::Redacted,
        )
        .unwrap();
        assert_eq!(
            prospective.lookback_days,
            REMOTE_COLLECTION_MAX_LOOKBACK_DAYS
        );

        assert!(
            remote_worker_config(&base, &range(now, now), now, RedactionProfile::Redacted,)
                .is_err()
        );
        assert!(
            remote_worker_config(
                &base,
                &range(now - Duration::days(35) - Duration::seconds(1), now),
                now,
                RedactionProfile::Redacted,
            )
            .is_err()
        );
    }

    #[test]
    fn partial_scan_cannot_publish_smaller_aggregates_or_tombstones() {
        let temp = tempfile::tempdir().unwrap();
        let now = at(12, 0);
        let mut records = session_records(now);
        records.push(json!({
            "timestamp": now.to_rfc3339(),
            "type": "event_msg",
            "payload": {
                "type": "token_count",
                "info": {"total_token_usage": {
                    "input_tokens": 40,
                    "cached_input_tokens": 20,
                    "output_tokens": 10,
                    "reasoning_output_tokens": 5,
                    "total_tokens": 50
                }}
            }
        }));
        write_rollout(temp.path(), &records);
        let config = CollectConfig {
            codex_home: temp.path().to_owned(),
            rollout_cache_dir: Some(temp.path().join("cache")),
            ..CollectConfig::default()
        };

        let collected = collect_remote_rollouts(
            &config,
            &range(now - Duration::days(7), now),
            now,
            RedactionProfile::Redacted,
        )
        .unwrap();

        // The usable row demonstrates why merely choosing UpsertOnly is not
        // enough: this smaller same-key aggregate could otherwise replace a
        // previously complete value.
        assert_eq!(collected.dataset.calls.len(), 1);
        assert!(!collected.scan_complete);
        assert!(
            collected
                .partial_reasons
                .iter()
                .any(|reason| reason == PARTIAL_AMBIGUOUS_RESETS)
        );
        assert!(collected.aggregate_publication().is_none());
    }
}
