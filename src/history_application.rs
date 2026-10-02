//! History application boundary shared by one-shot reports and TUI refreshes.
//!
//! Preparation owns staging, persistence and recovery/cutover. Queries accept an
//! already prepared runtime and never collect, synchronize, migrate or flush.
//! Compatibility adapters retain the existing report warnings and source rules.

use crate::config::CollectConfig;
use crate::domain::{Provenance, TaskRecord};
use crate::history::{HistoryData, HistoryObservation, HistoryStore, SummaryBackfillAttempt};
use crate::history_ownership::{HistoryOwnershipState, OwnershipManifestStatus};
use crate::history_profile_lease::{
    HistoryProfileLeaseGuard, TryHistoryProfileLease, try_acquire_history_profile_lease,
};
use crate::history_query::{
    HistoryQueryContext, HistorySourceSelection, HistorySourceSelector,
    SOURCE_SELECTION_UNAVAILABLE_WARNING, UnifiedHistorySnapshot,
};
use crate::history_runtime::{HistoryRuntime, HistoryRuntimeWriteReport};
use crate::perf::HistoryMetrics;
use crate::service::{
    TryRecorderInstanceLock, default_status_file, incompatible_recorder_for_cutover,
    try_acquire_recorder_instance_lock,
};
use crate::snapshot::{CollectionResult, collect_snapshot};
use crate::source_export::LocalSessionDigestEvidence;
use crate::source_history::{
    RedactionProfile, SourceHistoryRemoteActiveRef, SourceKind, SourceMetadata,
};
use crate::summary_report::{
    history_view_since, retain_summary_backfill_evidence_buckets, summary_backfill_config,
    summary_backfill_scan_complete, summary_history_coverage_complete,
};
use chrono::{DateTime, Utc};
use std::borrow::Cow;
use std::io;
use std::path::PathBuf;
use std::time::Instant;

pub(crate) enum ReportHistoryStore {
    Runtime {
        runtime: Box<HistoryRuntime>,
        profile_lease: Option<HistoryProfileLeaseGuard>,
    },
    LegacyFallback {
        store: Box<HistoryStore>,
        writable: bool,
    },
}

impl ReportHistoryStore {
    pub(crate) fn legacy_history(&self) -> &HistoryStore {
        match self {
            Self::Runtime { runtime, .. } => runtime.legacy_history(),
            Self::LegacyFallback { store, .. } => store,
        }
    }

    pub(crate) fn validated_write_permitted(&self) -> io::Result<bool> {
        match self {
            Self::Runtime {
                runtime,
                profile_lease: Some(profile_lease),
            } => {
                profile_lease.validate()?;
                runtime_history_write_permitted(runtime)
            }
            Self::Runtime {
                profile_lease: None,
                ..
            } => Ok(false),
            Self::LegacyFallback { writable, .. } => Ok(*writable),
        }
    }
}

pub(crate) fn acquire_runtime_profile_lease(
    runtime: &HistoryRuntime,
) -> io::Result<HistoryProfileLeaseGuard> {
    match try_acquire_history_profile_lease(
        runtime.state_root(),
        runtime.profile_id().clone(),
        runtime.redaction_profile(),
    )? {
        TryHistoryProfileLease::Acquired(guard) => Ok(guard),
        TryHistoryProfileLease::Busy { active_profile } => {
            let detail = active_profile.map_or_else(
                || "a profile transition is in progress".to_owned(),
                |active| {
                    format!(
                        "the active history selection uses {:?}",
                        active.redaction_profile()
                    )
                },
            );
            Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                format!("{detail}; retry after the other process exits"),
            ))
        }
    }
}

pub(crate) fn runtime_requires_v2_cutover(runtime: &HistoryRuntime) -> io::Result<bool> {
    Ok(match runtime.ownership().load_manifest()? {
        OwnershipManifestStatus::Uninitialized => true,
        OwnershipManifestStatus::Initialized(manifest) => {
            manifest.state() != HistoryOwnershipState::V2Active || !manifest.is_sqlite_backend()
        }
    })
}

/// File-based v2 is retained as upgrade input and a read-only snapshot.
/// V1 may still be owned by the original recorder while cutover is deferred.
pub(crate) fn runtime_history_write_permitted(runtime: &HistoryRuntime) -> io::Result<bool> {
    Ok(match runtime.ownership().load_manifest()? {
        OwnershipManifestStatus::Initialized(manifest) => {
            if manifest.state() == HistoryOwnershipState::V1Active {
                true
            } else if manifest.state() == HistoryOwnershipState::V2Active
                && manifest.is_sqlite_backend()
            {
                let Some(database) = runtime.source_history().sqlite_database() else {
                    return Ok(false);
                };
                crate::sqlite_history_migration::validate_receipt(&database, &manifest)?;
                true
            } else {
                false
            }
        }
        OwnershipManifestStatus::Uninitialized => false,
    })
}

pub(crate) struct PreparedReportHistory {
    pub store: ReportHistoryStore,
    context: HistoryQueryContext,
    preparation: HistoryData,
    metrics: HistoryMetrics,
    started: Instant,
}

/// Stages and submits one observation regardless of how many selectors will be read.
pub(crate) fn prepare_report_history(
    config: &CollectConfig,
    result: &CollectionResult,
    history_dir: Option<PathBuf>,
) -> PreparedReportHistory {
    let started = Instant::now();
    let (mut store, mut warnings) = report_history_store(config, history_dir);
    let write_permitted = match store.validated_write_permitted() {
        Ok(permitted) => permitted,
        Err(error) => {
            warnings.push(format!("history persistence is read-only because its profile lease or active storage could not be revalidated: {error}"));
            false
        }
    };
    let observation = report_history_observation(result, config.offline);
    let stage_started = Instant::now();
    match &mut store {
        ReportHistoryStore::Runtime { runtime, .. } => {
            if let Some(warning) = stage_runtime_collection(
                runtime,
                &observation,
                &result.snapshot.tasks,
                &result.local_session_digests,
                false,
            ) {
                warnings.push(warning);
            }
        }
        ReportHistoryStore::LegacyFallback { store, .. } => store.stage(&observation),
    }
    let stage_elapsed = stage_started.elapsed();
    let record_started = Instant::now();
    // Validate after staging, immediately before a write. A long normalization
    // must not extend the lifetime of an earlier profile validation.
    let write_permitted = write_permitted
        && match store.validated_write_permitted() {
            Ok(permitted) => permitted,
            Err(error) => {
                warnings.push(format!("history persistence is read-only because its profile lease or active storage could not be revalidated: {error}"));
                false
            }
        };
    let mut preparation = HistoryData {
        warnings,
        read_only: !write_permitted,
        ..HistoryData::default()
    };
    let mut metrics = HistoryMetrics::default();
    match &mut store {
        ReportHistoryStore::Runtime { runtime, .. } => {
            let write = if write_permitted {
                runtime.flush_staged()
            } else {
                Ok(None)
            };
            apply_runtime_write_metrics(&mut metrics, &write);
            merge_runtime_history_write_result(&mut preparation, &write, "history persistence");
        }
        ReportHistoryStore::LegacyFallback { store, .. } => {
            let write = if write_permitted {
                store.flush_staged()
            } else {
                Ok(None)
            };
            apply_legacy_write_metrics(&mut metrics, &write);
            merge_history_write_result(&mut preparation, write);
        }
    }
    metrics.record_us = u64::try_from(record_started.elapsed().as_micros()).unwrap_or(u64::MAX);
    metrics.stage_us = u64::try_from(stage_elapsed.as_micros()).unwrap_or(u64::MAX);
    metrics.record_performed = write_permitted;
    PreparedReportHistory {
        store,
        context: HistoryQueryContext::new(history_view_since(result.snapshot.as_of)),
        preparation,
        metrics,
        started,
    }
}

impl PreparedReportHistory {
    #[cfg(test)]
    pub(crate) fn quota_loads(&self) -> usize {
        self.context.quota_loads
    }
    /// Read only. Reusing this request for another source shares quota and its
    /// read budget, and cannot repeat preparation or submission.
    pub fn query(
        &mut self,
        config: &CollectConfig,
        selector: &HistorySourceSelector,
    ) -> HistoryData {
        let load_started = Instant::now();
        let since = self.context.since();
        let mut history = match &mut self.store {
            ReportHistoryStore::Runtime { runtime, .. } => {
                let selection = selector.resolve(runtime.source_identity().node_id());
                match query_runtime_history(runtime, &selection, since, Some(&mut self.context)) {
                    Ok(snapshot) => snapshot.history,
                    Err(error) => {
                        let mut history = HistoryData::default();
                        if !matches!(selector, HistorySourceSelector::Remote(_)) {
                            runtime
                                .legacy_history()
                                .overlay_staged_since(&mut history, since);
                        }
                        history
                            .warnings
                            .push(format!("history query failed: {error}"));
                        history
                    }
                }
            }
            ReportHistoryStore::LegacyFallback { store, .. } => {
                legacy_history_for_source_selector(store.load_since_with_staged(since), selector)
            }
        };
        history.read_only |= self.preparation.read_only;
        history
            .warnings
            .extend(self.preparation.warnings.iter().cloned());
        let mut metrics = self.metrics.clone();
        metrics.duration_us = u64::try_from(self.started.elapsed().as_micros()).unwrap_or(u64::MAX);
        metrics.load_us = u64::try_from(load_started.elapsed().as_micros()).unwrap_or(u64::MAX);
        metrics.load_performed = true;
        metrics.quota_points = u64::try_from(history.quota_points.len()).unwrap_or(u64::MAX);
        metrics.local_buckets = u64::try_from(history.half_hour_buckets.len()).unwrap_or(u64::MAX);
        metrics.weekly_local_points =
            u64::try_from(history.weekly_local_points.len()).unwrap_or(u64::MAX);
        metrics.warnings = metrics
            .warnings
            .max(u64::try_from(history.warnings.len()).unwrap_or(u64::MAX));
        metrics.read_only |= history.read_only;
        config.perf_log.record_history(metrics);
        // Submission is recorded once, even if this request reads several sources.
        self.metrics.record_performed = false;
        normalize_history_warnings(&mut history);
        history
    }
}

/// Shared staging policy for report preparation, TUI refresh and Summary backfill.
pub(crate) fn stage_runtime_collection(
    runtime: &mut HistoryRuntime,
    observation: &HistoryObservation,
    tasks: &[TaskRecord],
    evidence: &LocalSessionDigestEvidence,
    full: bool,
) -> Option<String> {
    #[cfg(test)]
    STAGE_CALLS.with(|calls| calls.set(calls.get() + 1));
    let staged = if full {
        runtime.stage_full_local_collection(observation, tasks, evidence)
    } else {
        runtime.stage_local_collection(observation, tasks, evidence)
    };
    let Err(error) = staged else {
        return None;
    };
    let normalized = runtime.prepare_local_collection_observation(observation, tasks);
    if full {
        runtime.stage_full_observation(&normalized);
    } else {
        runtime.stage(&normalized);
    }
    Some(if full {
        format!("local session digest evidence could not be staged for reconciliation: {error}")
    } else {
        format!("local session digest evidence could not be staged: {error}")
    })
}

#[cfg(test)]
thread_local! {
    static STAGE_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn stage_calls() -> usize {
    STAGE_CALLS.with(std::cell::Cell::get)
}

pub(crate) fn query_runtime_history(
    runtime: &mut HistoryRuntime,
    selection: &HistorySourceSelection,
    since: DateTime<Utc>,
    context: Option<&mut HistoryQueryContext>,
) -> io::Result<UnifiedHistorySnapshot> {
    if let Some(context) = context {
        if context.since() != since {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "history request query range changed",
            ));
        }
        runtime.query_history_with_staged_selected(selection, context)
    } else {
        runtime.query_history_with_staged_selected(selection, &mut HistoryQueryContext::new(since))
    }
}

pub(crate) fn legacy_history_for_source_selector(
    history: HistoryData,
    source_selector: &HistorySourceSelector,
) -> HistoryData {
    let HistorySourceSelector::Remote(source_id) = source_selector else {
        return history;
    };
    let mut selected = HistoryData {
        quota_points: history.quota_points,
        read_only: history.read_only,
        ..HistoryData::default()
    };
    selected.warnings.extend(history.warnings);
    selected.warnings.push(format!(
        "{SOURCE_SELECTION_UNAVAILABLE_WARNING}:unsupported_by_legacy:{source_id}"
    ));
    selected
}

fn report_history_store(
    config: &CollectConfig,
    history_dir: Option<PathBuf>,
) -> (ReportHistoryStore, Vec<String>) {
    let explicit_root = history_dir;
    let runtime = explicit_root.as_ref().map_or_else(
        || HistoryRuntime::discover(&config.codex_home, config.redact_content),
        |history_root| {
            HistoryRuntime::new(
                history_root.clone(),
                &config.codex_home,
                config.redact_content,
            )
        },
    );
    match runtime {
        Ok(mut runtime) => {
            let (profile_lease, mut warnings) = match acquire_runtime_profile_lease(&runtime) {
                Ok(guard) => (Some(guard), Vec::new()),
                Err(error) => (
                    None,
                    vec![format!(
                        "history persistence is read-only because the requested profile cannot be selected: {error}"
                    )],
                ),
            };
            if profile_lease.is_some() {
                warnings.extend(prepare_report_history_runtime(&mut runtime));
            }
            (
                ReportHistoryStore::Runtime {
                    runtime: Box::new(runtime),
                    profile_lease,
                },
                warnings,
            )
        }
        Err(error) => {
            let store = HistoryStore::memory_only(&config.codex_home, config.redact_content);
            (
                ReportHistoryStore::LegacyFallback {
                    store: Box::new(store),
                    writable: false,
                },
                vec![format!(
                    "source-aware history runtime unavailable; showing only this process's collected history in a read-only memory view; no history will be persisted until the source-aware state is repaired: {error}"
                )],
            )
        }
    }
}

fn prepare_report_history_runtime(runtime: &mut HistoryRuntime) -> Vec<String> {
    let mut warnings = Vec::new();
    match runtime_requires_v2_cutover(runtime) {
        Ok(false) => {
            if let Err(error) = runtime.refresh_active_sqlite_backend() {
                warnings.push(format!(
                    "active history storage could not be verified: {error}"
                ));
            }
            return warnings;
        }
        Err(error) => {
            warnings.push(format!(
                "source-aware history ownership could not be inspected: {error}"
            ));
            return warnings;
        }
        Ok(true) => {}
    }

    let history_root = runtime
        .legacy_history()
        .history_root()
        .expect("a bound runtime always has a legacy root");
    let _cutover_guard = match try_acquire_recorder_instance_lock(history_root) {
        Ok(TryRecorderInstanceLock::Acquired(guard)) => guard,
        Ok(TryRecorderInstanceLock::Busy) => {
            if let Err(error) = runtime.ensure_ownership_initialized() {
                warnings.push(format!("history ownership initialization failed: {error}"));
            }
            warnings.push(
                "source-aware history cutover deferred while another recorder owns this state"
                    .to_owned(),
            );
            return warnings;
        }
        Err(error) => {
            if let Err(initialization_error) = runtime.ensure_ownership_initialized() {
                warnings.push(format!(
                    "history ownership initialization failed: {initialization_error}"
                ));
            }
            warnings.push(format!(
                "source-aware history cutover deferred because its recorder lock could not be verified: {error}"
            ));
            return warnings;
        }
    };

    let status_path = default_status_file(history_root);
    let incompatible = incompatible_recorder_for_cutover(
        &status_path,
        runtime.legacy_history().namespace(),
        Utc::now(),
    );
    match incompatible {
        Ok(Some(status)) => {
            if let Err(error) = runtime.ensure_ownership_initialized() {
                warnings.push(format!("history ownership initialization failed: {error}"));
            }
            warnings.push(format!(
                "source-aware history cutover deferred while legacy recorder pid {} may still be active",
                status.pid
            ));
        }
        Ok(None) => {
            if let Err(error) = runtime.ensure_v2_active() {
                warnings.push(format!("source-aware history cutover failed: {error}"));
            }
        }
        Err(error) => {
            if let Err(initialization_error) = runtime.ensure_ownership_initialized() {
                warnings.push(format!(
                    "history ownership initialization failed: {initialization_error}"
                ));
            }
            warnings.push(format!(
                "source-aware history cutover deferred because recorder status could not be verified at {}: {error}",
                status_path.display()
            ));
        }
    }
    warnings
}

fn apply_legacy_write_metrics(
    metrics: &mut HistoryMetrics,
    write_result: &io::Result<Option<crate::history::HistoryWriteReport>>,
) {
    match write_result {
        Ok(Some(report)) => {
            metrics.shards_written = u64::try_from(report.shards_written).unwrap_or(u64::MAX);
            metrics.shards_skipped = u64::try_from(report.shards_skipped).unwrap_or(u64::MAX);
            metrics.shards_pruned = u64::try_from(report.shards_pruned).unwrap_or(u64::MAX);
            metrics.warnings = u64::try_from(report.warnings.len()).unwrap_or(u64::MAX);
            metrics.read_only = report.read_only;
        }
        Ok(None) => {}
        Err(_) => metrics.warnings = 1,
    }
}

pub(crate) fn apply_runtime_write_metrics(
    metrics: &mut HistoryMetrics,
    write_result: &io::Result<Option<HistoryRuntimeWriteReport>>,
) {
    match write_result {
        Ok(Some(HistoryRuntimeWriteReport::V1(report))) => {
            metrics.shards_written = u64::try_from(report.shards_written).unwrap_or(u64::MAX);
            metrics.shards_skipped = u64::try_from(report.shards_skipped).unwrap_or(u64::MAX);
            metrics.shards_pruned = u64::try_from(report.shards_pruned).unwrap_or(u64::MAX);
            metrics.warnings = u64::try_from(report.warnings.len()).unwrap_or(u64::MAX);
            metrics.read_only = report.read_only;
        }
        Ok(Some(HistoryRuntimeWriteReport::V2(report))) => {
            metrics.shards_written = u64::try_from(
                report
                    .account
                    .shards_written
                    .saturating_add(report.buckets.shards_written)
                    .saturating_add(report.weekly.shards_written)
                    .saturating_add(report.session_digests.shards_written),
            )
            .unwrap_or(u64::MAX);
            metrics.shards_skipped = u64::try_from(
                report
                    .account
                    .shards_skipped
                    .saturating_add(report.buckets.shards_skipped)
                    .saturating_add(report.weekly.shards_skipped)
                    .saturating_add(report.session_digests.shards_skipped),
            )
            .unwrap_or(u64::MAX);
        }
        Ok(None) => {}
        Err(_) => metrics.warnings = 1,
    }
}

pub(crate) fn report_history_observation(
    result: &CollectionResult,
    offline: bool,
) -> Cow<'_, HistoryObservation> {
    let account_limits_are_fresh = result
        .account
        .limits
        .iter()
        .any(|limit| limit.provenance == Provenance::ServerSnapshot);
    if offline || account_limits_are_fresh {
        return Cow::Borrowed(&result.history_observation);
    }
    let mut observation = result.history_observation.clone();
    observation.quota_points.clear();
    observation.weekly_local_points.clear();
    Cow::Owned(observation)
}

fn merge_history_write_result(
    history: &mut HistoryData,
    write_result: io::Result<Option<crate::history::HistoryWriteReport>>,
) {
    match write_result {
        Ok(Some(report)) => {
            history.read_only |= report.read_only;
            history.warnings.extend(report.warnings);
        }
        Ok(None) => {}
        Err(error) => history
            .warnings
            .push(format!("history persistence failed: {error}")),
    }
}

pub(crate) fn merge_runtime_history_write_result(
    history: &mut HistoryData,
    write_result: &io::Result<Option<HistoryRuntimeWriteReport>>,
    operation: &str,
) {
    match write_result {
        Ok(Some(HistoryRuntimeWriteReport::V1(report))) => {
            history.read_only |= report.read_only;
            history.warnings.extend(report.warnings.iter().cloned());
        }
        Ok(Some(HistoryRuntimeWriteReport::V2(_))) | Ok(None) => {}
        Err(error) => history
            .warnings
            .push(format!("{operation} failed: {error}")),
    }
}

/// Normalizes a Summary scan without importing fallback account quota.
/// Collection and scheduling remain with the one-shot or interactive caller.
pub(crate) struct SummaryBackfillObservation {
    pub scan_complete: bool,
    pub observed_at: DateTime<Utc>,
    pub tasks: Vec<TaskRecord>,
    pub local_session_digests: LocalSessionDigestEvidence,
    pub observation: HistoryObservation,
}

impl SummaryBackfillObservation {
    pub(crate) fn from_collection(result: CollectionResult) -> Self {
        let scan_complete = summary_backfill_scan_complete(&result.snapshot);
        let mut observation = result.history_observation;
        observation.quota_points.clear();
        observation.weekly_local_points.clear();
        retain_summary_backfill_evidence_buckets(&mut observation);
        Self {
            scan_complete,
            observed_at: result.snapshot.as_of,
            tasks: result.snapshot.tasks,
            local_session_digests: result.local_session_digests,
            observation,
        }
    }
}

pub(crate) fn summary_backfill_attempt(
    history: &HistoryData,
    observed_at: DateTime<Utc>,
    scan_complete: bool,
) -> SummaryBackfillAttempt {
    SummaryBackfillAttempt {
        completed_at: observed_at,
        complete: scan_complete && summary_history_coverage_complete(history, observed_at),
    }
}

/// Keep an in-memory cooldown even when persisting the marker fails, so a
/// read-only or full state directory does not trigger an immediate rescan.
pub(crate) fn apply_summary_backfill_marker(
    history: &mut HistoryData,
    requested: SummaryBackfillAttempt,
    marker: io::Result<SummaryBackfillAttempt>,
) {
    let marker = marker.unwrap_or_else(|error| {
        history
            .warnings
            .push(format!("summary backfill marker failed: {error}"));
        requested
    });
    history.summary_backfill_attempted_at = Some(marker.completed_at);
    history.summary_backfill_attempt_complete = Some(marker.complete);
}

pub(crate) fn backfill_summary_history_selected(
    config: &CollectConfig,
    store: &mut ReportHistoryStore,
    source_selector: &HistorySourceSelector,
) -> (HistoryData, DateTime<Utc>) {
    let worker_config = summary_backfill_config(config);
    let SummaryBackfillObservation {
        scan_complete,
        observed_at,
        tasks,
        local_session_digests,
        observation,
    } = SummaryBackfillObservation::from_collection(collect_snapshot(&worker_config, None, false));
    let since = history_view_since(observed_at);
    let (write_permitted, mut profile_validation_warning) = match store.validated_write_permitted()
    {
        Ok(permitted) => (permitted, None),
        Err(error) => (
            false,
            Some(format!(
                "summary backfill persistence is read-only because its profile lease could not be revalidated: {error}"
            )),
        ),
    };
    let mut history = match store {
        ReportHistoryStore::Runtime { runtime, .. } => {
            if let Some(warning) = stage_runtime_collection(
                runtime,
                &observation,
                &tasks,
                &local_session_digests,
                true,
            ) {
                profile_validation_warning.get_or_insert(warning);
            }
            let write_result = if write_permitted {
                runtime.flush_staged_reconcile(since, observed_at)
            } else {
                Ok(None)
            };
            let selection = source_selector.resolve(runtime.source_identity().node_id());
            let mut history = match query_runtime_history(runtime, &selection, since, None) {
                Ok(snapshot) => snapshot.history,
                Err(error) => {
                    let mut history = HistoryData::default();
                    if !matches!(source_selector, HistorySourceSelector::Remote(_)) {
                        runtime
                            .legacy_history()
                            .overlay_staged_since(&mut history, since);
                    }
                    history
                        .warnings
                        .push(format!("summary backfill history query failed: {error}"));
                    history
                }
            };
            merge_runtime_history_write_result(
                &mut history,
                &write_result,
                "summary backfill persistence",
            );
            history
        }
        ReportHistoryStore::LegacyFallback { store, writable } => {
            store.stage_full_observation(&observation);
            let write_result = if *writable {
                store.flush_staged()
            } else {
                Ok(None)
            };
            let mut history = legacy_history_for_source_selector(
                store.load_since_with_staged(since),
                source_selector,
            );
            match write_result {
                Ok(Some(report)) => {
                    history.read_only |= report.read_only;
                    history.warnings.extend(report.warnings);
                }
                Ok(None) => {}
                Err(error) => history
                    .warnings
                    .push(format!("summary backfill persistence failed: {error}")),
            }
            history
        }
    };
    if !write_permitted {
        history.read_only = true;
        if matches!(store, ReportHistoryStore::Runtime { .. }) {
            history.warnings.push(
                "summary backfill persistence deferred because this process does not hold the active history profile lease"
                    .to_owned(),
            );
        }
    }
    if let Some(warning) = profile_validation_warning {
        history.warnings.push(warning);
    }
    let requested = summary_backfill_attempt(&history, observed_at, scan_complete);
    let requested_complete = requested.complete;
    let marker_write_permitted = match store.validated_write_permitted() {
        Ok(permitted) => permitted,
        Err(error) => {
            history.warnings.push(format!(
                "summary backfill marker is read-only because its profile lease could not be revalidated: {error}"
            ));
            false
        }
    };
    let marker = match store {
        ReportHistoryStore::Runtime { runtime, .. } if marker_write_permitted => {
            runtime.mark_summary_backfill_attempt(observed_at, requested_complete)
        }
        ReportHistoryStore::Runtime { .. } => Ok(crate::history::SummaryBackfillAttempt {
            completed_at: observed_at,
            complete: requested_complete,
        }),
        ReportHistoryStore::LegacyFallback { store, writable }
            if marker_write_permitted && *writable =>
        {
            store.mark_summary_backfill_attempt(observed_at, requested_complete)
        }
        ReportHistoryStore::LegacyFallback { .. } => Ok(crate::history::SummaryBackfillAttempt {
            completed_at: observed_at,
            complete: requested_complete,
        }),
    };
    apply_summary_backfill_marker(&mut history, requested, marker);
    normalize_history_warnings(&mut history);
    (history, observed_at)
}

pub(crate) fn normalize_history_warnings(history: &mut HistoryData) {
    history.warnings.sort();
    history.warnings.dedup();
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HistoryProjectionRevision {
    pub ownership: OwnershipManifestStatus,
    pub project_mapping_revision: u64,
    pub local_observation_revision: u64,
    pub other_local_observation_revision: u64,
    pub sources: Vec<(SourceMetadata, Option<SourceHistoryRemoteActiveRef>)>,
}

impl HistoryProjectionRevision {
    pub fn same_query_inputs_except_local_revision(&self, other: &Self) -> bool {
        self.ownership == other.ownership
            && self.project_mapping_revision == other.project_mapping_revision
            && self.other_local_observation_revision == other.other_local_observation_revision
            && self.sources == other.sources
    }
}

pub(crate) fn history_projection_revision(
    runtime: &HistoryRuntime,
    selection: &HistorySourceSelection,
) -> io::Result<Option<HistoryProjectionRevision>> {
    let Some(before) = history_projection_revision_once(runtime, selection)? else {
        return Ok(None);
    };
    let Some(after) = history_projection_revision_once(runtime, selection)? else {
        return Ok(None);
    };
    Ok((before == after).then_some(after))
}

fn history_projection_revision_once(
    runtime: &HistoryRuntime,
    selection: &HistorySourceSelection,
) -> io::Result<Option<HistoryProjectionRevision>> {
    let ownership = runtime.ownership().load_manifest()?;
    if !matches!(
        &ownership,
        OwnershipManifestStatus::Initialized(manifest)
            if manifest.state() == HistoryOwnershipState::V2Active
    ) {
        return Ok(None);
    }
    if let OwnershipManifestStatus::Initialized(manifest) = &ownership {
        if manifest.is_sqlite_backend() != runtime.source_history().sqlite_database().is_some() {
            // A different process activated SQLite after this runtime was
            // constructed. Force a query, which refreshes the facade, instead
            // of stamping a cached file projection with the new ownership.
            return Ok(None);
        }
        if manifest.is_sqlite_backend() {
            crate::sqlite_history_migration::validate_receipt(
                &runtime.source_history().sqlite_database().unwrap(),
                manifest,
            )?;
        }
    }
    let project_mapping_revision = match runtime.project_mapping_store().load() {
        Ok(mappings) => mappings.revision(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => 0,
        Err(error) => return Err(error),
    };
    let local_observation_revision = runtime
        .source_history()
        .load_local_observation_projection_revision(
            runtime.source_identity(),
            runtime.redaction_profile(),
        )?;
    // Account quota is shared by the profile. The other privacy writer may
    // update it without changing this runtime's own observation namespace or
    // source metadata, so it must also invalidate a cached projection.
    let other_local_observation_revision = if runtime.source_history().sqlite_database().is_some() {
        let redaction = match runtime.redaction_profile() {
            RedactionProfile::Redacted => RedactionProfile::PreviewEnabled,
            RedactionProfile::PreviewEnabled => RedactionProfile::Redacted,
        };
        runtime
            .source_history()
            .load_local_observation_projection_revision(runtime.source_identity(), redaction)?
    } else {
        0
    };
    let mut metadata = runtime.source_history().list_source_metadata()?;
    metadata.sort_by(|left, right| left.source_id().as_str().cmp(right.source_id().as_str()));
    let selected = metadata.into_iter().filter(|source| {
        let usage_selected = match selection {
            HistorySourceSelection::AllIncluded => source.include_in_aggregates(),
            HistorySourceSelection::Local(source_id)
            | HistorySourceSelection::Remote(source_id) => source.source_id() == source_id,
        };
        let quota_selected = source.kind() == SourceKind::Ssh
            && source.include_in_aggregates()
            && !source.detached()
            && source.quota_matches_local_account();
        usage_selected || quota_selected
    });
    let sources = selected
        .map(|source| {
            let active = (source.kind() == SourceKind::Ssh
                && !(runtime.redaction_profile() == RedactionProfile::Redacted
                    && source.aggregate_redaction_profile() == RedactionProfile::PreviewEnabled))
                .then(|| {
                    runtime.source_history().active_remote_history_ref(
                        source.source_id(),
                        source.aggregate_redaction_profile(),
                    )
                })
                .transpose()?
                .flatten();
            Ok((source, active))
        })
        .collect::<io::Result<Vec<_>>>()?;
    Ok(Some(HistoryProjectionRevision {
        ownership,
        project_mapping_revision,
        local_observation_revision,
        other_local_observation_revision,
        sources,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{AccountSnapshot, CollectionStats, Snapshot, TokenUsage, UsageCall};
    use crate::summary_report::summary_history_backfill_needed;

    #[test]
    fn summary_backfill_scan_keeps_usage_and_tasks_without_importing_account_history() {
        let mut snapshot: Snapshot =
            serde_json::from_str(include_str!("../tests/fixtures/snapshots/normal.json")).unwrap();
        snapshot.stats = CollectionStats {
            discovered_files: 1,
            scanned_files: 1,
            ..CollectionStats::default()
        };
        let now = snapshot.as_of;
        let call = UsageCall {
            timestamp: now - chrono::Duration::minutes(15),
            thread_id: snapshot.tasks[0].thread_id.clone(),
            turn_id: None,
            usage_event_id: None,
            usage_event_identity_exact: false,
            model: Some("gpt-5.6-sol".to_owned()),
            service_tier: None,
            tokens: TokenUsage {
                total_tokens: 10,
                ..TokenUsage::default()
            },
            request_usage_exact: true,
        };
        let observation = HistoryObservation::from_sources_with_tasks_and_coverage(
            now,
            &[call],
            &[],
            &snapshot.limits,
            &[],
            Some(now - chrono::Duration::days(30)),
        );
        assert!(!observation.quota_points.is_empty());
        assert!(!observation.weekly_local_points.is_empty());
        let original_bucket_count = observation.half_hour_buckets.len();
        let expected_tasks = snapshot.tasks.clone();
        let backfill = SummaryBackfillObservation::from_collection(CollectionResult {
            snapshot,
            account: AccountSnapshot::default(),
            history_observation: observation,
            local_session_digests: LocalSessionDigestEvidence::default(),
        });

        assert!(backfill.scan_complete);
        assert_eq!(backfill.tasks, expected_tasks);
        assert!(backfill.observation.quota_points.is_empty());
        assert!(backfill.observation.weekly_local_points.is_empty());
        assert!(backfill.observation.half_hour_buckets.len() < original_bucket_count);
        assert_eq!(backfill.observation.half_hour_buckets.len(), 1);
        assert_eq!(backfill.observation.half_hour_buckets[0].call_count, 1);
        assert_eq!(
            backfill.observation.half_hour_buckets[0]
                .token_usage
                .total_tokens,
            10
        );
    }

    #[test]
    fn summary_backfill_marker_failure_prevents_an_immediate_rescan() {
        let now = DateTime::parse_from_rfc3339("2026-09-26T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let mut history = HistoryData::default();
        assert!(summary_history_backfill_needed(&history, now));
        let requested = summary_backfill_attempt(&history, now, true);
        assert!(!requested.complete);
        apply_summary_backfill_marker(
            &mut history,
            requested,
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "read-only fixture",
            )),
        );

        assert!(!summary_history_backfill_needed(&history, now));
        assert!(
            history
                .warnings
                .iter()
                .any(|warning| { warning == "summary backfill marker failed: read-only fixture" })
        );
    }
}
