//! Aggregate Delta exporter orchestration for one short-lived remote-agent
//! request.
//!
//! The caller owns framing. This module keeps collection, materialization,
//! durable reconcile, and page reading under one source/profile transaction,
//! then exposes immutable page prefixes so the runtime can enforce the final
//! negotiated compressed-frame size without rescanning rollouts.

use std::collections::HashSet;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::time::{Duration as StdDuration, SystemTime};

use chrono::{DateTime, Duration, Utc};
use walkdir::WalkDir;

use crate::config::CollectConfig;
use crate::domain::{TaskStatus, TurnStatus};
use crate::git_repository::GitProjectEvidenceResolver;
use crate::remote_collection::collect_remote_rollouts;
use crate::remote_delta_journal::{
    RemoteDeltaJournalPageInputs, RemoteDeltaPageDecodeError,
    decode_remote_delta_journal_page_classified, validate_remote_delta_desired_records,
};
use crate::remote_export_plan::plan_remote_export_records;
use crate::remote_export_state::{
    RemoteDeltaCursorExpired, RemoteDeltaPageRead, RemoteExportDeltaPage, RemoteExportLivePage,
    RemoteExportStateStore, RemoteRevisionFenceMode, try_acquire_revision_fence,
};
use crate::remote_protocol::{
    DeltaCursor, DeltaPage, DeltaPayload, DeltaRequest, MAX_LIVE_SERIALIZED_BYTES, MAX_LIVE_TASKS,
    MAX_LIVE_TURNS, ProtocolRevisions, RemoteDeltaCoverage, RemoteDeltaWarning,
    RemoteProjectDescriptor, SourceGeneration,
};
use crate::source_export::{
    MaterializedLiveSnapshot, materialize_live_snapshot_with_git_resolver,
    materialize_source_observation_with_git_resolver,
};
use crate::source_history::RedactionProfile;
use crate::source_identity::{SourceIdentity, SourceIdentityStore};

const MAX_DURABLE_PAGE_ENTRIES: usize = 4_096;
const MAX_DURABLE_PAGE_BYTES: u64 = 8 * 1024 * 1024;
const REVISION_NAMESPACE: &str = "remote-export-revisions-v2";
const HISTORICAL_COVERAGE_UNPROVEN: &str = "historical_coverage_unproven";
const LIVE_SNAPSHOT_TRUNCATED: &str = "live_snapshot_truncated";
const LIVE_SNAPSHOT_LOOKBACK_HOURS: i64 = 24;
const GIT_EVIDENCE_CACHE_FILE: &str = "git-evidence-cache-v1.json";
const EXPORT_LOCK_FILE_NAME: &str = "remote-export.lock";
const REVISION_FENCE_LOCK_FILE_NAME: &str = "remote-revision.lock";
const FACT_BATCH_NAMESPACE: &str = "session-fact-batches-v1";
const FROZEN_BATCH_TTL: StdDuration = StdDuration::from_secs(24 * 60 * 60);
const OLD_REVISION_GRACE: StdDuration = FROZEN_BATCH_TTL;
const MAX_RETAINED_OLD_REVISIONS: usize = 2;
const MAX_REVISION_DIRECTORY_ENTRIES: usize = 32_768;
const MAX_REVISION_DELETIONS_PER_REQUEST: usize = 2;
// At the minimum one-minute scheduler interval, repeatedly replacing a fully
// occupied snapshot remains below 96 MiB/day before compression. Aggregate
// history pages and SSH framing are budgeted separately by the center.

/// A fully collected and durably reconciled page that can be decoded at any
/// prefix length without reopening state or rescanning source rollouts.
#[derive(Clone, Debug)]
pub struct PreparedRemoteDeltaPage {
    durable_page: RemoteExportDeltaPage,
    inputs: RemoteDeltaJournalPageInputs,
}

impl PreparedRemoteDeltaPage {
    pub fn observed_at(&self) -> DateTime<Utc> {
        self.inputs.observed_at
    }

    pub fn entry_count(&self) -> usize {
        self.durable_page.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.durable_page.entries.is_empty()
    }

    /// Decodes the full durable page.
    pub fn decode(&self) -> Result<(DeltaPage, DeltaPayload), RemoteDeltaPageDecodeError> {
        self.decode_prefix(self.entry_count())
    }

    /// Decodes the first `entry_limit` journal transitions. A nonempty page
    /// must retain at least one entry; emitting a zero-progress `hasMore` page
    /// would violate cursor semantics and could make a center spin forever.
    pub fn decode_prefix(
        &self,
        entry_limit: usize,
    ) -> Result<(DeltaPage, DeltaPayload), RemoteDeltaPageDecodeError> {
        if self.durable_page.entries.is_empty() {
            if entry_limit != 0 {
                return Err(RemoteDeltaPageDecodeError::fatal_message(
                    "empty remote delta page requires a zero entry limit",
                ));
            }
            return decode_remote_delta_journal_page_classified(
                &self.durable_page,
                self.inputs.clone(),
            );
        }
        if entry_limit == 0 || entry_limit > self.durable_page.entries.len() {
            return Err(RemoteDeltaPageDecodeError::fatal_message(
                "remote delta page prefix is outside its durable entry bounds",
            ));
        }

        let mut page = self.durable_page.clone();
        if entry_limit < page.entries.len() {
            page.entries.truncate(entry_limit);
            let through_sequence = page
                .entries
                .last()
                .expect("a positive page prefix is nonempty")
                .sequence();
            page.through_sequence = through_sequence;
            page.next_cursor = DeltaCursor {
                generation: page.generation,
                sequence: through_sequence,
            };
            page.has_more = true;
        }
        decode_remote_delta_journal_page_classified(&page, self.inputs.clone())
    }
}

/// Error classes safe for the remote runtime to map to bounded framed
/// failures. Internal details stay local and must not be serialized because
/// they may contain source paths.
#[derive(Debug)]
pub enum RemoteDeltaPrepareError {
    Busy,
    CursorExpired(RemoteDeltaCursorExpired),
    Internal(anyhow::Error),
}

impl fmt::Display for RemoteDeltaPrepareError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Busy => formatter.write_str("remote exporter is already running"),
            Self::CursorExpired(_) => formatter.write_str("remote delta cursor expired"),
            Self::Internal(_) => formatter.write_str("remote delta export failed"),
        }
    }
}

impl std::error::Error for RemoteDeltaPrepareError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Internal(error) => Some(error.as_ref()),
            Self::Busy | Self::CursorExpired(_) => None,
        }
    }
}

/// Collects and prepares one aggregate page. `observed_at` is fixed before
/// scanning so future writes cannot leak into an earlier response watermark.
pub fn prepare_remote_delta_page(
    config: &CollectConfig,
    identity_store: &SourceIdentityStore,
    identity: &SourceIdentity,
    revisions: &ProtocolRevisions,
    redaction_profile: RedactionProfile,
    request: &DeltaRequest,
    observed_at: DateTime<Utc>,
) -> Result<PreparedRemoteDeltaPage, RemoteDeltaPrepareError> {
    let source = source_generation(identity);
    let state_root = revision_bound_state_root(identity_store, revisions)
        .map_err(RemoteDeltaPrepareError::Internal)?;
    let _revision_fence = try_acquire_revision_fence(&state_root, RemoteRevisionFenceMode::Shared)
        .map_err(map_state_error)?;
    let git_evidence_cache_path = state_root.join(GIT_EVIDENCE_CACHE_FILE);
    let state_store = RemoteExportStateStore::new(state_root, source.clone(), redaction_profile);
    let mut session = state_store
        .try_begin(observed_at)
        .map_err(map_state_error)?;

    // A continuation cursor addresses the already durable, source/profile
    // journal. Serve pending entries before touching rollout files: pagination
    // must remain available even when a later collection is slow or fails, and
    // one backlog must not trigger the same full scan for every page.
    if let Some(cursor) = request.delta_cursor {
        match session
            .read_page(cursor, MAX_DURABLE_PAGE_ENTRIES, MAX_DURABLE_PAGE_BYTES)
            .map_err(map_state_error)?
        {
            RemoteDeltaPageRead::Page(durable_page) if !durable_page.entries.is_empty() => {
                let live = if request.include_live {
                    session
                        .current_live_page(request.known_live_revision)
                        .map_err(map_state_error)?
                } else {
                    None
                };
                if request.include_live && live.is_none() {
                    // This exporter predates live state. Establish one from a
                    // single scan before serving the durable continuation.
                } else {
                    return Ok(PreparedRemoteDeltaPage {
                        durable_page,
                        inputs: pending_journal_page_inputs(
                            source,
                            revisions,
                            redaction_profile,
                            request,
                            observed_at,
                            live,
                        ),
                    });
                }
            }
            RemoteDeltaPageRead::Page(_) => {}
            RemoteDeltaPageRead::CursorExpired(expired) => {
                return Err(RemoteDeltaPrepareError::CursorExpired(expired));
            }
        }
    }

    // A pending durable page above remains a filesystem-only fast path. Run
    // bounded revision maintenance only when this request is already about to
    // pay for a complete rollout collection/materialization.
    sweep_old_revision_state(identity_store, revisions, SystemTime::now())
        .map_err(RemoteDeltaPrepareError::Internal)?;

    let collection =
        collect_remote_rollouts(config, &request.range, observed_at, redaction_profile)
            .map_err(RemoteDeltaPrepareError::Internal)?;

    let mut coverage_reasons = collection.partial_reasons.clone();
    coverage_reasons.push(HISTORICAL_COVERAGE_UNPROVEN.to_owned());
    coverage_reasons.sort_unstable();
    coverage_reasons.dedup();
    let stats = collection.delta_stats.clone();
    let mut warnings = collection.delta_warnings.clone();
    // Live and aggregate payloads are projections of the same rollout scan.
    // Sharing one resolver keeps both under one subprocess/time budget and
    // reuses exact cwd/repository evidence between the projections.
    let mut git_project_evidence =
        GitProjectEvidenceResolver::with_persistent_cache(git_evidence_cache_path);
    git_project_evidence.begin_collection();
    let live = if request.include_live {
        let materialized = materialize_live_snapshot_with_git_resolver(
            identity,
            redaction_profile,
            &collection.dataset.tasks,
            &collection.dataset.turns,
            observed_at,
            &mut git_project_evidence,
        )
        .map_err(|error| RemoteDeltaPrepareError::Internal(error.into()))?;
        // Active/uncertain tasks plus the latest 24 hours are the defined
        // live-view scope. Omitting older terminal rows is normal, not partial
        // collection evidence; only invalid rows or hard capacity truncation
        // below degrade live quality.
        let (materialized, _, _) = window_live_snapshot(materialized, observed_at);
        let (materialized, bounded_tasks, bounded_turns) = bound_live_snapshot(materialized)
            .map_err(|error| RemoteDeltaPrepareError::Internal(error.into()))?;
        let dropped_tasks = materialized.dropped_tasks.saturating_add(bounded_tasks);
        let dropped_turns = materialized.dropped_turns.saturating_add(bounded_turns);
        if dropped_tasks > 0 || dropped_turns > 0 {
            coverage_reasons.push(LIVE_SNAPSHOT_TRUNCATED.to_owned());
            add_warning(
                &mut warnings,
                LIVE_SNAPSHOT_TRUNCATED,
                dropped_tasks.saturating_add(dropped_turns),
            );
        }
        coverage_reasons.sort_unstable();
        coverage_reasons.dedup();
        warnings.sort_by(|left, right| left.code.cmp(&right.code));
        let live_partial_reasons = coverage_reasons
            .iter()
            .filter(|reason| reason.as_str() != HISTORICAL_COVERAGE_UNPROVEN)
            .cloned()
            .collect();
        Some(
            session
                .reconcile_live_snapshot(
                    observed_at,
                    materialized.snapshot,
                    materialized.project_descriptors,
                    live_partial_reasons,
                    warnings.clone(),
                    request.known_live_revision,
                )
                .map_err(map_state_error)?,
        )
    } else {
        None
    };
    coverage_reasons.sort_unstable();
    coverage_reasons.dedup();
    warnings.sort_by(|left, right| left.code.cmp(&right.code));
    let (live_state, live_project_descriptors) = live
        .map(|live| (Some(live.state), live.project_descriptors))
        .unwrap_or_default();
    let mut inputs = RemoteDeltaJournalPageInputs {
        source: source.clone(),
        redaction_profile,
        revisions: revisions.clone(),
        observed_at,
        coverage: RemoteDeltaCoverage {
            requested_range: request.range.clone(),
            covered_range: None,
            range_complete: false,
            partial_reasons: coverage_reasons,
        },
        live: live_state,
        stats,
        warnings,
        live_project_descriptors,
    };

    if let Some(publication) = collection.aggregate_publication() {
        let materialized = materialize_source_observation_with_git_resolver(
            identity,
            redaction_profile,
            &collection.dataset.tasks,
            &collection.dataset.calls,
            publication.observation().clone(),
            true,
            &mut git_project_evidence,
        )
        .map_err(|error| RemoteDeltaPrepareError::Internal(error.into()))?;
        let desired = plan_remote_export_records(&materialized)
            .map_err(|error| RemoteDeltaPrepareError::Internal(error.into()))?;
        validate_remote_delta_desired_records(&desired, &inputs)
            .map_err(|error| RemoteDeltaPrepareError::Internal(error.into()))?;
        session
            .reconcile_materialized_records(observed_at, &desired, publication.reconcile_mode())
            .map_err(map_state_error)?;
    }

    let git_stats = git_project_evidence.collection_stats();
    inputs.stats.git_commands_spawned = git_stats.commands;
    inputs.stats.git_workspaces_probed = git_stats.workspaces;
    inputs.stats.git_evidence_cache_hits = git_stats.cache_hits;
    inputs.stats.git_budget_exhausted = git_stats.budget_exhausted;
    inputs.stats.git_elapsed_millis = git_stats.elapsed_millis;

    let status = session.status().map_err(map_state_error)?;
    let cursor = request.delta_cursor.unwrap_or(DeltaCursor {
        generation: status.cursor.generation,
        sequence: status.retention_floor,
    });
    let durable_page = match session
        .read_page(cursor, MAX_DURABLE_PAGE_ENTRIES, MAX_DURABLE_PAGE_BYTES)
        .map_err(map_state_error)?
    {
        RemoteDeltaPageRead::Page(page) => page,
        RemoteDeltaPageRead::CursorExpired(expired) => {
            return Err(RemoteDeltaPrepareError::CursorExpired(expired));
        }
    };

    Ok(PreparedRemoteDeltaPage {
        durable_page,
        inputs,
    })
}

/// The aggregate journal intentionally scans the full 35-day retention
/// domain, but Overview live rows do not need to retransmit that inventory.
/// Keep active/uncertain rows regardless of age and terminal rows from the
/// latest day; turns are additionally bound to a retained task. The omitted
/// counts are carried as explicit live quality evidence.
fn window_live_snapshot(
    mut materialized: MaterializedLiveSnapshot,
    observed_at: DateTime<Utc>,
) -> (MaterializedLiveSnapshot, usize, usize) {
    let original_tasks = materialized.snapshot.tasks.len();
    let original_turns = materialized.snapshot.turns.len();
    let cutoff = observed_at
        .checked_sub_signed(Duration::hours(LIVE_SNAPSHOT_LOOKBACK_HOURS))
        .unwrap_or(DateTime::<Utc>::MIN_UTC);

    materialized.snapshot.tasks.retain(|task| {
        matches!(
            task.status,
            TaskStatus::Running
                | TaskStatus::WaitingApproval
                | TaskStatus::WaitingInput
                | TaskStatus::Unknown
        ) || task.updated_at >= cutoff
    });
    let retained_threads = materialized
        .snapshot
        .tasks
        .iter()
        .map(|task| task.thread_id.as_str())
        .collect::<HashSet<_>>();
    materialized.snapshot.turns.retain(|turn| {
        retained_threads.contains(turn.thread_id.as_str())
            && (matches!(turn.status, TurnStatus::InProgress | TurnStatus::Unknown)
                || turn
                    .completed_at
                    .or(turn.started_at)
                    .is_some_and(|at| at >= cutoff))
    });
    let retained_task_count = materialized.snapshot.tasks.len();
    let retained_turn_count = materialized.snapshot.turns.len();
    drop(retained_threads);
    let referenced = materialized
        .snapshot
        .tasks
        .iter()
        .filter_map(|task| task.observed_project_key.as_ref())
        .map(|key| key.as_str())
        .collect::<HashSet<_>>();
    materialized
        .project_descriptors
        .retain(|descriptor| referenced.contains(descriptor.observed_project_key.as_str()));
    drop(referenced);

    (
        materialized,
        original_tasks.saturating_sub(retained_task_count),
        original_turns.saturating_sub(retained_turn_count),
    )
}

fn pending_journal_page_inputs(
    source: SourceGeneration,
    revisions: &ProtocolRevisions,
    redaction_profile: RedactionProfile,
    request: &DeltaRequest,
    observed_at: DateTime<Utc>,
    live: Option<RemoteExportLivePage>,
) -> RemoteDeltaJournalPageInputs {
    let (live_state, live_project_descriptors, mut partial_reasons, warnings) = live
        .map(|live| {
            (
                Some(live.state),
                live.project_descriptors,
                live.partial_reasons,
                live.warnings,
            )
        })
        .unwrap_or_default();
    partial_reasons.push(HISTORICAL_COVERAGE_UNPROVEN.to_owned());
    partial_reasons.sort_unstable();
    partial_reasons.dedup();
    RemoteDeltaJournalPageInputs {
        source,
        redaction_profile,
        revisions: revisions.clone(),
        observed_at,
        coverage: RemoteDeltaCoverage {
            requested_range: request.range.clone(),
            covered_range: None,
            range_complete: false,
            partial_reasons,
        },
        live: live_state,
        stats: Default::default(),
        warnings,
        live_project_descriptors,
    }
}

fn bound_live_snapshot(
    mut materialized: MaterializedLiveSnapshot,
) -> io::Result<(MaterializedLiveSnapshot, usize, usize)> {
    let original_tasks = materialized.snapshot.tasks.len();
    let original_turns = materialized.snapshot.turns.len();
    let mut task_limit = original_tasks.min(MAX_LIVE_TASKS);
    let mut turn_limit = original_turns.min(MAX_LIVE_TURNS);

    loop {
        let mut tasks = materialized.snapshot.tasks.clone();
        tasks.sort_by(|left, right| {
            right
                .updated_at
                .cmp(&left.updated_at)
                .then_with(|| left.thread_id.as_str().cmp(right.thread_id.as_str()))
        });
        tasks.truncate(task_limit);
        tasks.sort_by(|left, right| left.thread_id.as_str().cmp(right.thread_id.as_str()));
        let retained_threads = tasks
            .iter()
            .map(|task| task.thread_id.as_str())
            .collect::<HashSet<_>>();
        let mut turns = materialized
            .snapshot
            .turns
            .iter()
            .filter(|turn| retained_threads.contains(turn.thread_id.as_str()))
            .cloned()
            .collect::<Vec<_>>();
        turns.sort_by(|left, right| {
            right
                .completed_at
                .or(right.started_at)
                .cmp(&left.completed_at.or(left.started_at))
                .then_with(|| left.thread_id.as_str().cmp(right.thread_id.as_str()))
                .then_with(|| left.turn_id.cmp(&right.turn_id))
        });
        turns.truncate(turn_limit);
        turns.sort_by(|left, right| {
            left.thread_id
                .as_str()
                .cmp(right.thread_id.as_str())
                .then_with(|| left.turn_id.cmp(&right.turn_id))
        });
        let referenced = tasks
            .iter()
            .filter_map(|task| task.observed_project_key.as_ref())
            .map(|key| key.as_str())
            .collect::<HashSet<_>>();
        let descriptors = materialized
            .project_descriptors
            .iter()
            .filter(|descriptor| referenced.contains(descriptor.observed_project_key.as_str()))
            .cloned()
            .collect::<Vec<RemoteProjectDescriptor>>();
        let encoded = serde_json::to_vec(&(&tasks, &turns, &descriptors))
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        if encoded.len() <= MAX_LIVE_SERIALIZED_BYTES || (task_limit == 0 && turn_limit == 0) {
            let retained_task_count = tasks.len();
            let retained_turn_count = turns.len();
            materialized.snapshot.tasks = tasks;
            materialized.snapshot.turns = turns;
            materialized.project_descriptors = descriptors;
            return Ok((
                materialized,
                original_tasks.saturating_sub(retained_task_count),
                original_turns.saturating_sub(retained_turn_count),
            ));
        }
        if turn_limit > 0 {
            turn_limit /= 2;
        } else if task_limit > 0 {
            task_limit /= 2;
        }
    }
}

fn add_warning(warnings: &mut Vec<RemoteDeltaWarning>, code: &str, occurrences: usize) {
    let occurrences = u64::try_from(occurrences).unwrap_or(u64::MAX).max(1);
    if let Some(existing) = warnings.iter_mut().find(|warning| warning.code == code) {
        let total = existing.occurrences.get().saturating_add(occurrences);
        existing.occurrences = NonZeroU64::new(total).expect("warning total is non-zero");
    } else {
        warnings.push(RemoteDeltaWarning {
            code: code.to_owned(),
            occurrences: NonZeroU64::new(occurrences).expect("warning count is non-zero"),
        });
    }
}

pub(crate) fn revision_bound_state_root(
    identity_store: &SourceIdentityStore,
    revisions: &ProtocolRevisions,
) -> Result<PathBuf, anyhow::Error> {
    let identity_path = identity_store
        .path()
        .ok_or_else(|| anyhow::anyhow!("remote source identity has no durable state path"))?;
    let state_directory = identity_path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    Ok(state_directory.join(REVISION_NAMESPACE).join(format!(
        "h{}-m{}-e{}-p{}-a{}",
        revisions.history_format,
        revisions.metric,
        revisions.estimator,
        revisions.project_breakdown,
        revisions.api_pricing_catalog,
    )))
}

#[derive(Debug)]
struct OldRevisionCandidate {
    path: PathBuf,
    modified: SystemTime,
    lock_paths: Vec<PathBuf>,
    active_frozen_continuation: bool,
    has_revision_fence: bool,
    retired: bool,
    saturated: bool,
}

/// Bounded maintenance for revision-qualified exporter state. The exact
/// current tuple is excluded before any recursive traversal. Older tuples are
/// removable only after every discovered exporter lock is acquired and no
/// unexpired frozen continuation exists.
pub(crate) fn sweep_old_revision_state(
    identity_store: &SourceIdentityStore,
    revisions: &ProtocolRevisions,
    now: SystemTime,
) -> Result<(), anyhow::Error> {
    let current_root = revision_bound_state_root(identity_store, revisions)?;
    let namespace = current_root
        .parent()
        .ok_or_else(|| anyhow::anyhow!("remote export revision root has no parent"))?;
    match validate_revision_directory(namespace) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    }

    let mut candidates = Vec::new();
    let mut namespace_saturated = false;
    for (index, entry) in fs::read_dir(namespace)?.enumerate() {
        if index == MAX_REVISION_DIRECTORY_ENTRIES {
            namespace_saturated = true;
            break;
        }
        let entry = entry?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        validate_revision_storage_entry(&path, &metadata)?;
        if !metadata.file_type().is_dir() {
            return Err(invalid_revision_data(
                "remote export revision namespace contains a non-directory entry",
            )
            .into());
        }
        if path == current_root {
            continue;
        }
        let name = entry
            .file_name()
            .to_str()
            .ok_or_else(|| invalid_revision_data("remote export revision name is not UTF-8"))?
            .to_owned();
        let retired = validate_revision_directory_name(&name)?;
        candidates.push(scan_old_revision_candidate(path, retired, now)?);
    }
    validate_revision_directory(namespace)?;

    candidates.sort_by(|left, right| {
        left.retired
            .cmp(&right.retired)
            .reverse()
            .then_with(|| left.modified.cmp(&right.modified))
            .then_with(|| left.path.cmp(&right.path))
    });
    let mut retained = candidates
        .iter()
        .filter(|candidate| !candidate.retired)
        .count();
    let mut deleted = 0_usize;
    for candidate in candidates {
        if deleted == MAX_REVISION_DELETIONS_PER_REQUEST {
            break;
        }
        if candidate.saturated || candidate.active_frozen_continuation {
            continue;
        }
        let expired = now
            .duration_since(candidate.modified)
            .is_ok_and(|age| age >= OLD_REVISION_GRACE);
        let over_capacity = namespace_saturated || retained > MAX_RETAINED_OLD_REVISIONS;
        // A pre-fence development build may still be serving this namespace.
        // Capacity pressure is not enough to cross that compatibility boundary;
        // require the full grace before installing/acquiring the new fence.
        if !candidate.has_revision_fence && !expired {
            continue;
        }
        if !candidate.retired && !expired && !over_capacity {
            continue;
        }
        let revision_fence =
            match try_acquire_revision_fence(&candidate.path, RemoteRevisionFenceMode::Exclusive) {
                Ok(fence) => fence,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => continue,
                Err(error) => return Err(error.into()),
            };
        let Some(locks) = try_lock_revision_exporters(&candidate.lock_paths)? else {
            continue;
        };
        // Lock discovery and validation are repeated while the locks are held.
        // A changed set is conservatively retained rather than deleting state
        // whose activity fence was not acquired.
        let revalidated =
            scan_old_revision_candidate(candidate.path.clone(), candidate.retired, now)?;
        if revalidated.saturated
            || revalidated.active_frozen_continuation
            || revalidated.lock_paths != candidate.lock_paths
        {
            continue;
        }

        #[cfg(windows)]
        {
            drop(locks);
            drop(revision_fence);
        }

        let removal_path = if candidate.retired {
            candidate.path.clone()
        } else {
            match retire_revision_directory(&candidate.path) {
                Ok(path) => path,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => continue,
                #[cfg(windows)]
                Err(error) if windows_revision_path_is_busy(&error) => continue,
                Err(error) => return Err(error.into()),
            }
        };
        let removed = match fs::remove_dir_all(&removal_path) {
            Ok(()) => true,
            Err(error) if error.kind() == io::ErrorKind::NotFound => true,
            #[cfg(windows)]
            Err(error) if windows_revision_path_is_busy(&error) => false,
            Err(error) => return Err(error.into()),
        };
        #[cfg(not(windows))]
        {
            drop(locks);
            drop(revision_fence);
        }
        if !removed {
            continue;
        }
        sync_revision_parent(&removal_path)?;
        if !candidate.retired {
            retained = retained.saturating_sub(1);
        }
        deleted = deleted.saturating_add(1);
    }
    Ok(())
}

fn scan_old_revision_candidate(
    path: PathBuf,
    retired: bool,
    now: SystemTime,
) -> Result<OldRevisionCandidate, anyhow::Error> {
    validate_revision_directory(&path)?;
    let mut modified = fs::symlink_metadata(&path)?.modified()?;
    let mut lock_paths = Vec::new();
    let mut active_frozen_continuation = false;
    let mut has_revision_fence = false;
    let mut saturated = false;
    for (index, entry) in WalkDir::new(&path)
        .min_depth(1)
        .follow_links(false)
        .into_iter()
        .enumerate()
    {
        if index == MAX_REVISION_DIRECTORY_ENTRIES {
            saturated = true;
            break;
        }
        let entry = entry?;
        let entry_path = entry.path();
        let metadata = fs::symlink_metadata(entry_path)?;
        validate_revision_storage_entry(entry_path, &metadata)?;
        modified = modified.max(metadata.modified()?);
        if metadata.file_type().is_file() && entry.file_name() == EXPORT_LOCK_FILE_NAME {
            lock_paths.push(entry_path.to_path_buf());
        }
        if entry.depth() == 1
            && metadata.file_type().is_file()
            && entry.file_name() == REVISION_FENCE_LOCK_FILE_NAME
        {
            has_revision_fence = true;
        }
        if entry.depth() == 5
            && metadata.file_type().is_dir()
            && is_frozen_batch_path(&path, entry_path)
            && !entry.file_name().to_string_lossy().starts_with(".gc-")
        {
            let batch_is_live = now
                .duration_since(metadata.modified()?)
                .map_or(true, |age| age < FROZEN_BATCH_TTL);
            active_frozen_continuation |= batch_is_live;
        }
    }
    validate_revision_directory(&path)?;
    lock_paths.sort();
    lock_paths.dedup();
    Ok(OldRevisionCandidate {
        path,
        modified,
        lock_paths,
        active_frozen_continuation,
        has_revision_fence,
        retired,
        saturated,
    })
}

fn is_frozen_batch_path(revision_root: &Path, path: &Path) -> bool {
    path.strip_prefix(revision_root)
        .ok()
        .and_then(|relative| relative.components().next())
        .is_some_and(|component| component.as_os_str() == FACT_BATCH_NAMESPACE)
}

fn validate_revision_directory_name(name: &str) -> io::Result<bool> {
    let (name, retired) = match name.strip_prefix(".gc-") {
        Some(name) => (name, true),
        None => (name, false),
    };
    let mut parts = name.split('-');
    let valid = [
        ("h", parts.next()),
        ("m", parts.next()),
        ("e", parts.next()),
        ("p", parts.next()),
        ("a", parts.next()),
    ]
    .into_iter()
    .all(|(prefix, part)| {
        part.and_then(|part| part.strip_prefix(prefix))
            .and_then(|number| number.parse::<u32>().ok())
            .is_some_and(|number| number > 0)
    }) && parts.next().is_none();
    if !valid {
        return Err(invalid_revision_data(
            "remote export revision directory name is invalid",
        ));
    }
    Ok(retired)
}

fn try_lock_revision_exporters(paths: &[PathBuf]) -> io::Result<Option<Vec<File>>> {
    let mut locks = Vec::with_capacity(paths.len());
    for path in paths {
        let Some(lock) = try_lock_existing_exporter(path)? else {
            return Ok(None);
        };
        locks.push(lock);
    }
    Ok(Some(locks))
}

/// Holds every exporter source lock found below one already validated state
/// subtree. A second bounded scan closes the ordinary create-between-scan
/// race; callers must retain this guard through rename/removal on Unix. On
/// Windows they drop it immediately before rename, where the existing lock
/// handles' lack of DELETE sharing provides the atomic activity fence.
pub(crate) struct RemoteExporterTreeLocks {
    _locks: Vec<File>,
}

pub(crate) fn try_lock_exporter_tree(
    root: &Path,
    maximum_entries: usize,
) -> io::Result<Option<RemoteExporterTreeLocks>> {
    let Some(paths) = scan_exporter_lock_paths(root, maximum_entries)? else {
        return Ok(None);
    };
    let Some(locks) = try_lock_revision_exporters(&paths)? else {
        return Ok(None);
    };
    let Some(revalidated) = scan_exporter_lock_paths(root, maximum_entries)? else {
        return Ok(None);
    };
    if revalidated != paths {
        return Ok(None);
    }
    Ok(Some(RemoteExporterTreeLocks { _locks: locks }))
}

fn scan_exporter_lock_paths(
    root: &Path,
    maximum_entries: usize,
) -> io::Result<Option<Vec<PathBuf>>> {
    validate_revision_directory(root)?;
    let mut paths = Vec::new();
    for (index, entry) in WalkDir::new(root)
        .min_depth(1)
        .follow_links(false)
        .into_iter()
        .enumerate()
    {
        if index == maximum_entries {
            return Ok(None);
        }
        let entry = entry.map_err(io::Error::other)?;
        let metadata = fs::symlink_metadata(entry.path())?;
        validate_revision_storage_entry(entry.path(), &metadata)?;
        if metadata.file_type().is_file() && entry.file_name() == EXPORT_LOCK_FILE_NAME {
            paths.push(entry.path().to_path_buf());
        }
    }
    validate_revision_directory(root)?;
    paths.sort();
    paths.dedup();
    Ok(Some(paths))
}

fn try_lock_existing_exporter(path: &Path) -> io::Result<Option<File>> {
    let before = fs::symlink_metadata(path)?;
    validate_revision_storage_entry(path, &before)?;
    if !before.file_type().is_file() {
        return Err(invalid_revision_data(
            "remote exporter lock is not a regular file",
        ));
    }
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    add_revision_nofollow_flags(&mut options);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};

        options.share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE);
    }
    let file = options.open(path)?;
    validate_open_revision_lock(path, &file, &before)?;
    match fs2::FileExt::try_lock_exclusive(&file) {
        Ok(()) => {}
        Err(error) if lock_is_contended(&error) => return Ok(None),
        Err(error) => return Err(error),
    }
    validate_open_revision_lock(path, &file, &before)?;
    Ok(Some(file))
}

fn lock_is_contended(error: &io::Error) -> bool {
    let expected = fs2::lock_contended_error();
    error.kind() == expected.kind()
        && (error.raw_os_error().is_none()
            || expected.raw_os_error().is_none()
            || error.raw_os_error() == expected.raw_os_error())
}

fn validate_open_revision_lock(path: &Path, file: &File, _before: &fs::Metadata) -> io::Result<()> {
    let opened = file.metadata()?;
    validate_revision_storage_entry(path, &opened)?;
    let current = fs::symlink_metadata(path)?;
    validate_revision_storage_entry(path, &current)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        if _before.dev() != opened.dev()
            || _before.ino() != opened.ino()
            || current.dev() != opened.dev()
            || current.ino() != opened.ino()
        {
            return Err(invalid_revision_data(
                "remote exporter lock changed while it was being acquired",
            ));
        }
    }
    #[cfg(windows)]
    crate::source_identity::validate_windows_private_file(
        path,
        file,
        "remote exporter revision lock",
    )?;
    #[cfg(not(any(unix, windows)))]
    return Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "remote revision GC is unsupported on this platform",
    ));
    Ok(())
}

fn add_revision_nofollow_flags(options: &mut OpenOptions) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;

        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
}

fn retire_revision_directory(path: &Path) -> io::Result<PathBuf> {
    let parent = path
        .parent()
        .ok_or_else(|| invalid_revision_data("remote revision has no parent directory"))?;
    validate_revision_directory(parent)?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| invalid_revision_data("remote revision name is not UTF-8"))?;
    if validate_revision_directory_name(name)? {
        return Ok(path.to_path_buf());
    }
    let retired = parent.join(format!(".gc-{name}"));
    match fs::symlink_metadata(&retired) {
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "a retired remote revision already exists",
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    fs::rename(path, &retired)?;
    validate_revision_directory(&retired)?;
    validate_revision_directory(parent)?;
    sync_revision_parent(&retired)?;
    Ok(retired)
}

fn validate_revision_directory(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if revision_metadata_is_link_or_reparse(&metadata) || !metadata.file_type().is_dir() {
        return Err(invalid_revision_data(
            "remote export revision path is not a private directory",
        ));
    }
    validate_revision_storage_entry(path, &metadata)
}

fn validate_revision_storage_entry(_path: &Path, metadata: &fs::Metadata) -> io::Result<()> {
    if revision_metadata_is_link_or_reparse(metadata)
        || !(metadata.file_type().is_dir() || metadata.file_type().is_file())
    {
        return Err(invalid_revision_data(
            "remote export revision contains a link or special entry",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        // SAFETY: geteuid has no preconditions and retains no pointers.
        let effective_uid = unsafe { libc::geteuid() };
        if metadata.uid() != effective_uid || metadata.permissions().mode() & 0o077 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "remote export revision entry is not private to the current user",
            ));
        }
    }
    #[cfg(windows)]
    if metadata.file_type().is_dir() {
        crate::source_identity::validate_windows_private_directory(
            _path,
            "remote export revision directory",
        )?;
    }
    #[cfg(windows)]
    if metadata.file_type().is_file() {
        let mut options = OpenOptions::new();
        options.read(true);
        add_revision_nofollow_flags(&mut options);
        let file = options.open(_path)?;
        crate::source_identity::validate_windows_private_file(
            _path,
            &file,
            "remote export revision file",
        )?;
    }
    Ok(())
}

#[cfg(unix)]
fn revision_metadata_is_link_or_reparse(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(windows)]
fn revision_metadata_is_link_or_reparse(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(any(unix, windows)))]
fn revision_metadata_is_link_or_reparse(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(windows)]
fn windows_revision_path_is_busy(error: &io::Error) -> bool {
    matches!(error.raw_os_error(), Some(5 | 32 | 33))
}

#[cfg(unix)]
fn sync_revision_parent(path: &Path) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| invalid_revision_data("remote revision path has no parent"))?;
    File::open(parent)?.sync_all()
}

#[cfg(not(unix))]
fn sync_revision_parent(_path: &Path) -> io::Result<()> {
    Ok(())
}

fn invalid_revision_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

/// Probe the exact revision-bound exporter namespace used by Delta requests,
/// rather than only the source-identity parent directory.
pub(crate) fn probe_remote_export_state_writable(
    identity_store: &SourceIdentityStore,
    revisions: &ProtocolRevisions,
) -> Result<(), anyhow::Error> {
    let state_root = revision_bound_state_root(identity_store, revisions)?;
    crate::cache::probe_private_directory_writable(&state_root).map_err(Into::into)
}

fn source_generation(identity: &SourceIdentity) -> SourceGeneration {
    SourceGeneration {
        node_id: identity.node_id().clone(),
        generation: std::num::NonZeroU64::new(identity.generation())
            .expect("validated source identities have a non-zero generation"),
    }
}

fn map_state_error(error: io::Error) -> RemoteDeltaPrepareError {
    if error.kind() == io::ErrorKind::WouldBlock {
        RemoteDeltaPrepareError::Busy
    } else {
        RemoteDeltaPrepareError::Internal(error.into())
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::num::{NonZeroU32, NonZeroU64};
    use std::time::Duration as StdDuration;

    use chrono::{Duration, TimeZone};
    use serde_json::json;

    use super::*;
    use crate::remote_agent::current_revisions;
    use crate::remote_protocol::{
        ExportRange, RemoteLiveSnapshot, RemoteLiveTask, RemoteLiveTurn, RemoteTokenUsage,
    };

    fn at(hour: u32, minute: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 30, hour, minute, 0)
            .single()
            .unwrap()
    }

    fn request(now: DateTime<Utc>) -> DeltaRequest {
        DeltaRequest {
            delta_cursor: None,
            range: ExportRange {
                from: now - Duration::days(7),
                to: now,
            },
            overlap_minutes: 60,
            include_live: false,
            known_live_revision: None,
        }
    }

    fn write_rollout(root: &Path, now: DateTime<Utc>) {
        let sessions = root.join("sessions/2026/08/30");
        fs::create_dir_all(&sessions).unwrap();
        let records = [
            json!({
                "timestamp": (now - Duration::minutes(3)).to_rfc3339(),
                "type": "session_meta",
                "payload": {
                    "id": "01a00000-0000-7000-8000-000000000001",
                    "timestamp": (now - Duration::minutes(3)).to_rfc3339(),
                    "cwd": root.join("project")
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
                "timestamp": (now - Duration::minutes(1)).to_rfc3339(),
                "type": "event_msg",
                "payload": {"type": "token_count", "info": {"total_token_usage": {
                    "input_tokens": 80,
                    "cached_input_tokens": 40,
                    "output_tokens": 20,
                    "reasoning_output_tokens": 10,
                    "total_tokens": 100
                }}}
            }),
        ];
        let contents = records
            .iter()
            .map(serde_json::Value::to_string)
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        fs::write(sessions.join("rollout-remote.jsonl"), contents).unwrap();
    }

    fn fixture() -> (
        tempfile::TempDir,
        CollectConfig,
        SourceIdentityStore,
        SourceIdentity,
        DateTime<Utc>,
    ) {
        let directory = tempfile::tempdir().unwrap();
        let now = at(12, 0);
        fs::create_dir(directory.path().join("project")).unwrap();
        write_rollout(directory.path(), now);
        let config = CollectConfig {
            codex_home: directory.path().to_owned(),
            rollout_cache_dir: Some(directory.path().join("cache")),
            active_grace: StdDuration::from_secs(300),
            ..CollectConfig::default()
        };
        let store =
            SourceIdentityStore::at_path(directory.path().join("state/source-identity.json"));
        let identity = store.load_or_create().unwrap();
        (directory, config, store, identity, now)
    }

    fn revision_variant(offset: u32) -> ProtocolRevisions {
        let mut revisions = current_revisions();
        revisions.metric = NonZeroU32::new(revisions.metric.get() + offset).unwrap();
        revisions
    }

    fn create_revision_state(
        store: &SourceIdentityStore,
        identity: &SourceIdentity,
        revisions: &ProtocolRevisions,
        observed_at: DateTime<Utc>,
    ) -> PathBuf {
        let root = revision_bound_state_root(store, revisions).unwrap();
        drop(try_acquire_revision_fence(&root, RemoteRevisionFenceMode::Shared).unwrap());
        let state = RemoteExportStateStore::new(
            root.clone(),
            source_generation(identity),
            RedactionProfile::Redacted,
        );
        drop(state.try_begin(observed_at).unwrap());
        root
    }

    fn future_after_revision_grace(paths: &[PathBuf]) -> SystemTime {
        let newest = paths
            .iter()
            .flat_map(|path| {
                WalkDir::new(path)
                    .follow_links(false)
                    .into_iter()
                    .map(|entry| {
                        fs::symlink_metadata(entry.unwrap().path())
                            .unwrap()
                            .modified()
                            .unwrap()
                    })
            })
            .max()
            .unwrap();
        newest
            .checked_add(OLD_REVISION_GRACE)
            .and_then(|time| time.checked_add(StdDuration::from_secs(1)))
            .unwrap()
    }

    fn create_private_test_tree(path: &Path) {
        fs::create_dir_all(path).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let mut current = Some(path);
            while let Some(directory) = current {
                if directory.ends_with("remote-export-revisions-v2") {
                    break;
                }
                fs::set_permissions(directory, fs::Permissions::from_mode(0o700)).unwrap();
                current = directory.parent();
            }
        }
    }

    #[test]
    fn revision_gc_never_removes_the_current_tuple() {
        let (_directory, _config, store, identity, now) = fixture();
        let revisions = current_revisions();
        let current = create_revision_state(&store, &identity, &revisions, now);
        let future = future_after_revision_grace(std::slice::from_ref(&current));

        sweep_old_revision_state(&store, &revisions, future).unwrap();

        assert!(current.exists());
    }

    #[test]
    fn revision_gc_retires_expired_state_and_finishes_crash_retirement() {
        let (_directory, _config, store, identity, now) = fixture();
        let current = current_revisions();
        let old = create_revision_state(&store, &identity, &revision_variant(1), now);
        let future = future_after_revision_grace(std::slice::from_ref(&old));

        let retired = retire_revision_directory(&old).unwrap();
        assert!(!old.exists());
        assert!(retired.exists());
        sweep_old_revision_state(&store, &current, future).unwrap();

        assert!(!retired.exists());
    }

    #[test]
    fn revision_gc_skips_an_active_revision_fence() {
        let (_directory, _config, store, identity, now) = fixture();
        let current = current_revisions();
        let old = create_revision_state(&store, &identity, &revision_variant(1), now);
        let future = future_after_revision_grace(std::slice::from_ref(&old));
        let _active = try_acquire_revision_fence(&old, RemoteRevisionFenceMode::Shared).unwrap();

        sweep_old_revision_state(&store, &current, future).unwrap();

        assert!(old.exists());
    }

    #[test]
    fn revision_gc_skips_an_active_child_exporter_lock() {
        let (_directory, _config, store, identity, now) = fixture();
        let current = current_revisions();
        let revisions = revision_variant(1);
        let old = revision_bound_state_root(&store, &revisions).unwrap();
        let state = RemoteExportStateStore::new(
            old.clone(),
            source_generation(&identity),
            RedactionProfile::PreviewEnabled,
        );
        let active = state.try_begin(now).unwrap();
        let future = future_after_revision_grace(std::slice::from_ref(&old));

        sweep_old_revision_state(&store, &current, future).unwrap();

        assert!(old.exists());
        drop(active);
    }

    #[test]
    fn revision_gc_never_uses_capacity_to_bypass_the_legacy_grace() {
        let (_directory, _config, store, identity, now) = fixture();
        let current = current_revisions();
        let roots = (1..=u32::try_from(MAX_RETAINED_OLD_REVISIONS + 1).unwrap())
            .map(|offset| {
                let root = revision_bound_state_root(&store, &revision_variant(offset)).unwrap();
                let state = RemoteExportStateStore::new(
                    root.clone(),
                    source_generation(&identity),
                    RedactionProfile::Redacted,
                );
                drop(state.try_begin(now).unwrap());
                assert!(!root.join(REVISION_FENCE_LOCK_FILE_NAME).exists());
                root
            })
            .collect::<Vec<_>>();

        sweep_old_revision_state(&store, &current, SystemTime::now()).unwrap();

        assert!(roots.iter().all(|root| root.exists()));
    }

    #[test]
    fn revision_gc_protects_a_fresh_frozen_continuation_under_capacity_pressure() {
        let (_directory, _config, store, identity, now) = fixture();
        let current = current_revisions();
        let roots = (1..=u32::try_from(MAX_RETAINED_OLD_REVISIONS + 1).unwrap())
            .map(|offset| create_revision_state(&store, &identity, &revision_variant(offset), now))
            .collect::<Vec<_>>();
        let protected = roots[0]
            .join(FACT_BATCH_NAMESPACE)
            .join("t-a44fe4c93f7307987391ecf96f80f74f7ad59d70423f46182ad294e99aa6a467")
            .join("retention-35")
            .join("snapshot")
            .join("fact-snapshot-live");
        create_private_test_tree(&protected);

        sweep_old_revision_state(&store, &current, SystemTime::now()).unwrap();

        assert!(roots[0].exists());
        assert!(protected.exists());
    }

    #[test]
    fn revision_gc_deletion_limit_converges_across_requests() {
        let (_directory, _config, store, identity, now) = fixture();
        let current = current_revisions();
        let roots = (1..=4)
            .map(|offset| create_revision_state(&store, &identity, &revision_variant(offset), now))
            .collect::<Vec<_>>();
        let future = future_after_revision_grace(&roots);

        sweep_old_revision_state(&store, &current, future).unwrap();
        assert_eq!(roots.iter().filter(|root| root.exists()).count(), 2);
        sweep_old_revision_state(&store, &current, future).unwrap();
        assert!(roots.iter().all(|root| !root.exists()));
    }

    #[test]
    fn complete_collection_reconciles_and_cursor_retry_is_empty() {
        let (_directory, config, store, identity, now) = fixture();
        let revisions = current_revisions();
        let first = prepare_remote_delta_page(
            &config,
            &store,
            &identity,
            &revisions,
            RedactionProfile::Redacted,
            &request(now),
            now,
        )
        .unwrap();
        assert!(first.entry_count() >= 2);
        let (first_page, first_payload) = first.decode().unwrap();
        assert_eq!(
            first_payload.stats.journal_records_scanned,
            first_payload.bucket_changes.len() as u64
                + first_payload.session_digest_changes.len() as u64
        );
        assert!(
            first_payload
                .coverage
                .partial_reasons
                .contains(&HISTORICAL_COVERAGE_UNPROVEN.to_owned())
        );

        let mut retry_request = request(now);
        retry_request.delta_cursor = Some(first_page.next_delta_cursor);
        let retry = prepare_remote_delta_page(
            &config,
            &store,
            &identity,
            &revisions,
            RedactionProfile::Redacted,
            &retry_request,
            now,
        )
        .unwrap();
        assert!(retry.is_empty());
        let (retry_page, retry_payload) = retry.decode().unwrap();
        assert_eq!(retry_page.next_delta_cursor, first_page.next_delta_cursor);
        assert_eq!(retry_payload.stats.journal_records_scanned, 0);
    }

    #[test]
    fn protocol_invalid_model_does_not_pollute_state_and_fixed_input_can_continue() {
        let (directory, config, store, identity, now) = fixture();
        let rollout = directory
            .path()
            .join("sessions/2026/08/30/rollout-remote.jsonl");
        let valid_contents = fs::read_to_string(&rollout).unwrap();
        let invalid_contents = valid_contents.replace("gpt-5.6-sol", &"x".repeat(257));
        fs::write(&rollout, invalid_contents).unwrap();
        let revisions = current_revisions();

        assert!(matches!(
            prepare_remote_delta_page(
                &config,
                &store,
                &identity,
                &revisions,
                RedactionProfile::Redacted,
                &request(now),
                now,
            ),
            Err(RemoteDeltaPrepareError::Internal(_))
        ));

        let state_root = revision_bound_state_root(&store, &revisions).unwrap();
        let state_store = RemoteExportStateStore::new(
            state_root,
            source_generation(&identity),
            RedactionProfile::Redacted,
        );
        let session = state_store.try_begin(now).unwrap();
        let status = session.status().unwrap();
        assert_eq!(status.retained_entries, 0);
        assert_eq!(status.materialized_records, 0);
        drop(session);

        fs::write(&rollout, valid_contents).unwrap();
        let fixed = prepare_remote_delta_page(
            &config,
            &store,
            &identity,
            &revisions,
            RedactionProfile::Redacted,
            &request(now),
            now,
        )
        .unwrap();
        assert!(!fixed.is_empty());
    }

    #[test]
    fn prepared_page_prefix_advances_contiguously_without_rescanning() {
        let (_directory, config, store, identity, now) = fixture();
        let prepared = prepare_remote_delta_page(
            &config,
            &store,
            &identity,
            &current_revisions(),
            RedactionProfile::Redacted,
            &request(now),
            now,
        )
        .unwrap();
        assert!(prepared.entry_count() > 1);

        let (prefix_page, prefix_payload) = prepared.decode_prefix(1).unwrap();
        assert!(prefix_page.has_more);
        assert_eq!(prefix_payload.stats.journal_records_scanned, 1);
        assert_eq!(
            prefix_payload.bucket_changes.len() + prefix_payload.session_digest_changes.len(),
            1
        );
        assert!(prepared.decode_prefix(0).is_err());
    }

    #[test]
    fn pending_continuation_survives_a_broken_later_collection() {
        let (directory, config, store, identity, now) = fixture();
        let revisions = current_revisions();
        let prepared = prepare_remote_delta_page(
            &config,
            &store,
            &identity,
            &revisions,
            RedactionProfile::Redacted,
            &request(now),
            now,
        )
        .unwrap();
        assert!(prepared.entry_count() > 1);
        let (prefix_page, _) = prepared.decode_prefix(1).unwrap();

        let rollout = directory
            .path()
            .join("sessions/2026/08/30/rollout-remote.jsonl");
        let invalid = fs::read_to_string(&rollout)
            .unwrap()
            .replace("gpt-5.6-sol", &"x".repeat(257));
        fs::write(rollout, invalid).unwrap();
        let revision_root = revision_bound_state_root(&store, &revisions).unwrap();
        create_private_test_tree(
            &revision_root
                .parent()
                .unwrap()
                .join("invalid-revision-that-must-not-be-scanned"),
        );

        let mut continuation = request(now);
        continuation.delta_cursor = Some(prefix_page.next_delta_cursor);
        let resumed = prepare_remote_delta_page(
            &config,
            &store,
            &identity,
            &revisions,
            RedactionProfile::Redacted,
            &continuation,
            now + Duration::minutes(1),
        )
        .unwrap();
        assert_eq!(resumed.entry_count(), prepared.entry_count() - 1);
        let (_, payload) = resumed.decode().unwrap();
        assert_eq!(payload.stats.discovered_files, 0);
        assert_eq!(payload.stats.parsed_files, 0);
        assert_eq!(
            payload.stats.journal_records_scanned,
            resumed.entry_count() as u64
        );
    }

    #[test]
    fn pending_continuation_replays_durable_live_quality_without_rescanning() {
        let (directory, config, store, identity, now) = fixture();
        let revisions = current_revisions();
        let mut initial_request = request(now);
        initial_request.include_live = true;
        let prepared = prepare_remote_delta_page(
            &config,
            &store,
            &identity,
            &revisions,
            RedactionProfile::Redacted,
            &initial_request,
            now,
        )
        .unwrap();
        assert!(prepared.entry_count() > 1);
        let (prefix_page, prefix_payload) = prepared.decode_prefix(1).unwrap();
        let initial_live = prefix_payload.live.unwrap();
        let revision = initial_live.live_revision;
        let snapshot = initial_live.snapshot.unwrap();

        // Model a later successful scan that found identical live rows but
        // degraded quality. This must update the durable quality without
        // changing the content revision so a no-scan continuation can replay
        // the current assessment exactly.
        let state_store = RemoteExportStateStore::new(
            revision_bound_state_root(&store, &revisions).unwrap(),
            source_generation(&identity),
            RedactionProfile::Redacted,
        );
        let warning = RemoteDeltaWarning {
            code: LIVE_SNAPSHOT_TRUNCATED.to_owned(),
            occurrences: NonZeroU64::new(4).unwrap(),
        };
        let mut state = state_store.try_begin(now + Duration::seconds(1)).unwrap();
        let quality_update = state
            .reconcile_live_snapshot(
                now + Duration::seconds(1),
                snapshot,
                prefix_payload.project_descriptors,
                vec![LIVE_SNAPSHOT_TRUNCATED.to_owned()],
                vec![warning.clone()],
                Some(revision),
            )
            .unwrap();
        assert_eq!(quality_update.state.live_revision, revision);
        assert!(quality_update.state.snapshot.is_none());
        drop(state);

        let rollout = directory
            .path()
            .join("sessions/2026/08/30/rollout-remote.jsonl");
        let invalid = fs::read_to_string(&rollout)
            .unwrap()
            .replace("gpt-5.6-sol", &"x".repeat(257));
        fs::write(rollout, invalid).unwrap();

        let mut continuation = request(now);
        continuation.include_live = true;
        continuation.delta_cursor = Some(prefix_page.next_delta_cursor);
        continuation.known_live_revision = Some(revision);
        let resumed = prepare_remote_delta_page(
            &config,
            &store,
            &identity,
            &revisions,
            RedactionProfile::Redacted,
            &continuation,
            now + Duration::minutes(1),
        )
        .unwrap();
        let (_, payload) = resumed.decode().unwrap();
        let resumed_live = payload.live.unwrap();
        assert_eq!(resumed_live.live_revision, revision);
        assert!(resumed_live.snapshot.is_none());
        assert!(
            payload
                .coverage
                .partial_reasons
                .contains(&HISTORICAL_COVERAGE_UNPROVEN.to_owned())
        );
        assert!(
            payload
                .coverage
                .partial_reasons
                .contains(&LIVE_SNAPSHOT_TRUNCATED.to_owned())
        );
        assert_eq!(payload.warnings, vec![warning]);
    }

    #[test]
    fn revision_tuple_changes_the_durable_delta_generation_namespace() {
        let (_directory, _config, store, _identity, _now) = fixture();
        let first = current_revisions();
        let mut second = first.clone();
        second.metric = NonZeroU32::new(first.metric.get() + 1).unwrap();

        let first_path = revision_bound_state_root(&store, &first).unwrap();
        let second_path = revision_bound_state_root(&store, &second).unwrap();
        assert_ne!(first_path, second_path);
        assert!(
            first_path
                .to_string_lossy()
                .contains("remote-export-revisions-v2")
        );
    }

    #[test]
    fn include_live_is_supported_and_expired_generation_fails_explicitly() {
        let (_directory, config, store, identity, now) = fixture();
        let revisions = current_revisions();
        let mut live = request(now);
        live.include_live = true;
        let prepared = prepare_remote_delta_page(
            &config,
            &store,
            &identity,
            &revisions,
            RedactionProfile::Redacted,
            &live,
            now,
        )
        .unwrap();
        let (_, payload) = prepared.decode().unwrap();
        assert!(payload.live.and_then(|live| live.snapshot).is_some());

        let mut expired = request(now);
        expired.delta_cursor = Some(DeltaCursor {
            generation: NonZeroU64::new(1).unwrap(),
            sequence: 0,
        });
        assert!(matches!(
            prepare_remote_delta_page(
                &config,
                &store,
                &identity,
                &revisions,
                RedactionProfile::Redacted,
                &expired,
                now,
            ),
            Err(RemoteDeltaPrepareError::CursorExpired(_))
        ));
    }

    #[test]
    fn live_replacement_retries_full_until_the_center_proves_its_exact_baseline() {
        let (_directory, config, store, identity, now) = fixture();
        let revisions = current_revisions();
        let mut first_request = request(now);
        first_request.include_live = true;
        let first = prepare_remote_delta_page(
            &config,
            &store,
            &identity,
            &revisions,
            RedactionProfile::Redacted,
            &first_request,
            now,
        )
        .unwrap();
        let (first_page, first_payload) = first.decode().unwrap();
        let first_live = first_payload.live.unwrap();
        let revision = first_live.live_revision;
        assert!(first_live.snapshot.is_some());

        // The response was lost: neither the aggregate cursor nor the local
        // live replacement advanced. The same durable revision must be sent
        // in full again rather than becoming revision-only exporter state.
        let lost_retry = prepare_remote_delta_page(
            &config,
            &store,
            &identity,
            &revisions,
            RedactionProfile::Redacted,
            &first_request,
            now + Duration::minutes(1),
        )
        .unwrap();
        let (_, lost_payload) = lost_retry.decode().unwrap();
        let lost_live = lost_payload.live.unwrap();
        assert_eq!(lost_live.live_revision, revision);
        assert!(lost_live.snapshot.is_some());

        // Once the center durably retains the exact replacement it can ask
        // for a compact revision-only response.
        let mut known = request(now);
        known.include_live = true;
        known.delta_cursor = Some(first_page.next_delta_cursor);
        known.known_live_revision = Some(revision);
        let unchanged = prepare_remote_delta_page(
            &config,
            &store,
            &identity,
            &revisions,
            RedactionProfile::Redacted,
            &known,
            now + Duration::minutes(2),
        )
        .unwrap();
        let (_, unchanged_payload) = unchanged.decode().unwrap();
        let unchanged_live = unchanged_payload.live.unwrap();
        assert_eq!(unchanged_live.live_revision, revision);
        assert!(unchanged_live.snapshot.is_none());

        // Losing only remote-live.json while retaining the aggregate cursor
        // makes knownLiveRevision absent on the next request. Full content is
        // therefore restored without rewinding aggregate history.
        known.known_live_revision = None;
        let missing_local_copy = prepare_remote_delta_page(
            &config,
            &store,
            &identity,
            &revisions,
            RedactionProfile::Redacted,
            &known,
            now + Duration::minutes(3),
        )
        .unwrap();
        let (_, restored_payload) = missing_local_copy.decode().unwrap();
        let restored_live = restored_payload.live.unwrap();
        assert_eq!(restored_live.live_revision, revision);
        assert!(restored_live.snapshot.is_some());
    }

    #[test]
    fn large_live_inventory_is_windowed_and_bounded_for_minute_sync() {
        let now = at(12, 0);
        let mut tasks = Vec::new();
        let mut turns = Vec::new();
        for index in 0..2_452_usize {
            let recent = index >= 2_400;
            let active = index == 0;
            let thread_id: crate::source_model::ThreadId =
                format!("thread-{index:04}").parse().unwrap();
            let updated_at = if recent {
                now - Duration::hours(1)
            } else {
                now - Duration::days(2)
            };
            tasks.push(RemoteLiveTask {
                thread_id: thread_id.clone(),
                parent_thread_id: None,
                observed_project_key: None,
                title_preview: Some("x".repeat(256)),
                created_at: Some(updated_at - Duration::minutes(1)),
                updated_at,
                status: if active {
                    TaskStatus::Running
                } else {
                    TaskStatus::Completed
                },
                token_usage: RemoteTokenUsage::default(),
                turn_count: 4,
            });
            for turn_index in 0..4 {
                turns.push(RemoteLiveTurn {
                    thread_id: thread_id.clone(),
                    turn_id: format!("turn-{turn_index}"),
                    model: Some("gpt-5.6-sol".to_owned()),
                    reasoning_effort: Some("high".to_owned()),
                    service_tier: None,
                    message_preview: Some("m".repeat(256)),
                    started_at: Some(updated_at),
                    completed_at: (!active).then_some(updated_at + Duration::seconds(1)),
                    status: if active {
                        TurnStatus::InProgress
                    } else {
                        TurnStatus::Completed
                    },
                    token_usage: RemoteTokenUsage::default(),
                });
            }
        }
        tasks.sort_by(|left, right| left.thread_id.as_str().cmp(right.thread_id.as_str()));
        turns.sort_by(|left, right| {
            left.thread_id
                .as_str()
                .cmp(right.thread_id.as_str())
                .then_with(|| left.turn_id.cmp(&right.turn_id))
        });
        let materialized = MaterializedLiveSnapshot {
            snapshot: RemoteLiveSnapshot {
                captured_at: now,
                tasks,
                turns,
            },
            project_descriptors: Vec::new(),
            dropped_tasks: 0,
            dropped_turns: 0,
        };

        let (windowed, dropped_tasks, dropped_turns) = window_live_snapshot(materialized, now);
        assert_eq!(windowed.snapshot.tasks.len(), 53);
        assert_eq!(dropped_tasks, 2_399);
        assert_eq!(windowed.snapshot.turns.len(), 212);
        assert_eq!(dropped_turns, 9_596);

        let (bounded, capacity_tasks, capacity_turns) = bound_live_snapshot(windowed).unwrap();
        assert!(bounded.snapshot.tasks.len() <= MAX_LIVE_TASKS);
        assert!(bounded.snapshot.turns.len() <= MAX_LIVE_TURNS);
        assert!(capacity_tasks > 0 || capacity_turns > 0);
        let encoded = serde_json::to_vec(&(
            &bounded.snapshot.tasks,
            &bounded.snapshot.turns,
            &bounded.project_descriptors,
        ))
        .unwrap();
        assert!(encoded.len() <= MAX_LIVE_SERIALIZED_BYTES);
        const {
            assert!(MAX_LIVE_SERIALIZED_BYTES * 24 * 60 <= 96 * 1024 * 1024);
        }
    }
}
