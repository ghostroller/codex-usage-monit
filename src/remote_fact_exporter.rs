//! Durable, fixed-watermark export of normalized per-thread usage facts.
//!
//! A complete rollout scan reconciles one source/profile/thread/retention
//! materialized set into the existing durable journal primitive. Snapshot and
//! delta starts then freeze an immutable batch in a separate durable journal;
//! continuation tokens address only that batch, so later source changes and
//! exporter restarts cannot change an in-flight page sequence.

use std::collections::HashSet;
use std::fmt;
use std::fs;
#[cfg(unix)]
use std::fs::File;
#[cfg(windows)]
use std::fs::OpenOptions;
use std::io;
use std::num::{NonZeroU64, TryFromIntError};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::{Duration as StdDuration, SystemTime};

use chrono::{DateTime, Days, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

use crate::config::CollectConfig;
use crate::domain::UsageCall;
use crate::history::HistoryObservation;
use crate::remote_collection::collect_remote_rollouts;
use crate::remote_export_state::{
    RemoteDeltaPageRead, RemoteExportChange, RemoteExportDesiredRecord, RemoteExportJournalEntry,
    RemoteExportReconcileDeltaPreflight, RemoteExportReconcileMode, RemoteExportSession,
    RemoteExportStateStore, RemoteRevisionFenceMode, try_acquire_revision_fence,
};
use crate::remote_exporter::revision_bound_state_root;
use crate::remote_protocol::{
    DeltaCursor, ExportRange, FactBatchId, FactCursor, FactDeltaPage, FactDeltaPageToken,
    FactSnapshotId, FactSnapshotPage, FactSnapshotPageToken, ProtocolRevisions,
    REMOTE_SESSION_FACT_SCHEMA_VERSION, RemoteFactDeltaPayload, RemoteFactSnapshotPayload,
    RemoteSessionFactPayload, RemoteUsageEventFact, RemoteUsageEventFactDeltaChange,
    RemoteUsageEventFactMutation, RemoteUsageEventFactRecord, SessionFactsDigestBinding,
    SessionFactsPosition, SessionFactsRequest, SourceGeneration,
};
use crate::source_export::{
    MaterializedSessionFacts, SessionFactMaterializationLimits, finalize_local_session_digests,
    is_session_fact_inventory_limit_error, materialize_local_session_digest_evidence,
    materialize_session_facts_from_normalized_observation_bounded, source_normalized_observation,
};
use crate::source_history::{RedactionProfile, SourceSessionDigest, UsageEventId};
use crate::source_identity::{SourceIdentity, SourceIdentityStore};
use crate::source_model::{SessionReplicaKey, ThreadId, ThreadShardKey};

const FACT_INDEX_NAMESPACE: &str = "session-fact-index-v1";
const FACT_BATCH_NAMESPACE: &str = "session-fact-batches-v1";
const FACT_BATCH_MAINTENANCE_NAMESPACE: &str = "session-fact-batch-maintenance-v1";
const BATCH_FORMAT_VERSION: u32 = 2;
const MAX_DURABLE_PAGE_ENTRIES: usize = 4_096;
const MAX_DURABLE_PAGE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_COMPLETE_BATCH_PAGES: usize = 8;
pub(crate) const MAX_COMPLETE_FACT_BATCH_RECORDS: usize =
    MAX_DURABLE_PAGE_ENTRIES * MAX_COMPLETE_BATCH_PAGES - 1;
const MAX_COMPLETE_FACT_BATCH_SERIALIZED_BYTES: usize = 8 * 1024 * 1024;
const MAX_FROZEN_FACT_RECORD_SERIALIZED_BYTES: usize = 64 * 1024;
// A wire fact record is smaller than its tagged FrozenBatchEntry. Keeping one
// emitted prefix under 2 MiB leaves a wide margin below the negotiated 4 MiB
// encoded-frame limit even for incompressible JSON and response metadata.
const MAX_SAFE_FACT_PAGE_SERIALIZED_BYTES: usize = 2 * 1024 * 1024;
// This is a revision/source-wide bound, not a per-thread bound. Together with
// the complete-batch byte limit it caps retained frozen payloads even when a
// center requests an unbounded sequence of distinct sessions.
const MAX_RETAINED_FROZEN_BATCHES_PER_KIND: usize = 128;
// Count every directory entry visited above a batch root, including empty
// structural directories left by an interrupted/legacy cleanup. This keeps a
// request's filesystem work bounded independently of the number of threads.
const MAX_FROZEN_BATCH_DIRECTORY_ENTRIES: usize = 4_096;
const MAX_FROZEN_BATCH_DELETIONS_PER_REQUEST: usize = 32;
const FROZEN_BATCH_TTL: StdDuration = StdDuration::from_secs(24 * 60 * 60);
// Indexes are recoverable through the protocol's typed cursor-expired path,
// but keeping a bounded recent set avoids forcing snapshots for ordinary
// 35-day use. The byte bound remains defense in depth: each index is already
// capped independently by `RemoteExportStateStore`.
const MAX_RETAINED_FACT_INDEXES: usize = 1_024;
const MAX_FACT_INDEX_DIRECTORY_ENTRIES: usize = 16_384;
const MAX_FACT_INDEX_DELETIONS_PER_REQUEST: usize = 16;
const FACT_INDEX_IN_FLIGHT_GRACE: StdDuration = FROZEN_BATCH_TTL;
const INDEX_FENCE_LOCK_FILE_NAME: &str = "remote-revision.lock";
const UPSERT_CHANGE_DOMAIN: &[u8] = b"codex-usage-monit/fact-upsert/v1\0";
const TOMBSTONE_CHANGE_DOMAIN: &[u8] = b"codex-usage-monit/fact-tombstone/v1\0";
const LOGICAL_KEY_DOMAIN: &[u8] = b"codex-usage-monit/fact-key/v1\0";

#[derive(Clone, Copy, Debug)]
struct CompleteFactBatchLimits {
    maximum_records: usize,
    maximum_record_serialized_bytes: usize,
    maximum_serialized_bytes: usize,
}

const COMPLETE_FACT_BATCH_LIMITS: CompleteFactBatchLimits = CompleteFactBatchLimits {
    maximum_records: MAX_COMPLETE_FACT_BATCH_RECORDS,
    maximum_record_serialized_bytes: MAX_FROZEN_FACT_RECORD_SERIALIZED_BYTES,
    maximum_serialized_bytes: MAX_COMPLETE_FACT_BATCH_SERIALIZED_BYTES,
};

/// Applies the same conservative record/count/serialized-byte envelope used
/// by frozen remote batches before either the remote index or a local active
/// generation is mutated. Only one record is cloned at a time.
#[cfg(test)]
pub(crate) fn validate_complete_fact_inventory(
    facts: &[RemoteUsageEventFact],
) -> Result<(), RemoteFactPrepareError> {
    if facts.len() > MAX_COMPLETE_FACT_BATCH_RECORDS {
        return Err(RemoteFactPrepareError::InventoryTooLarge);
    }
    // Reserve one maximum-sized entry for the frozen header. The real header
    // is checked again at freeze time; this keeps the preflight independent of
    // generated batch IDs and journal watermarks while remaining an upper
    // bound shared by local materialization.
    let mut serialized_bytes = MAX_FROZEN_FACT_RECORD_SERIALIZED_BYTES;
    for fact in facts {
        let payload_len = serialized_snapshot_fact_bytes(fact)
            .map_err(|error| RemoteFactPrepareError::Internal(error.into()))?;
        if payload_len > MAX_FROZEN_FACT_RECORD_SERIALIZED_BYTES {
            return Err(RemoteFactPrepareError::InventoryTooLarge);
        }
        add_serialized_batch_bytes(&mut serialized_bytes, payload_len)?;
    }
    Ok(())
}

/// Builds a fact inventory under exactly the same record/count/serialized-byte
/// envelope used by the frozen batch. Budget checks happen while unique event
/// candidates enter source normalization rather than after a full map and Vec
/// have already been allocated.
pub(crate) fn materialize_complete_session_facts_from_normalized_observation(
    requested_thread: &ThreadId,
    retention_days: u16,
    observed_at: DateTime<Utc>,
    normalized_observation: &HistoryObservation,
    calls: &[UsageCall],
    collection_partial_reasons: &[String],
) -> Result<MaterializedSessionFacts, RemoteFactPrepareError> {
    materialize_session_facts_from_normalized_observation_bounded(
        requested_thread,
        retention_days,
        observed_at,
        normalized_observation,
        calls,
        collection_partial_reasons,
        SessionFactMaterializationLimits {
            maximum_records: COMPLETE_FACT_BATCH_LIMITS.maximum_records,
            initial_serialized_bytes: MAX_FROZEN_FACT_RECORD_SERIALIZED_BYTES,
            maximum_record_serialized_bytes: COMPLETE_FACT_BATCH_LIMITS
                .maximum_record_serialized_bytes,
            maximum_serialized_bytes: COMPLETE_FACT_BATCH_LIMITS.maximum_serialized_bytes,
        },
        serialized_snapshot_fact_bytes,
    )
    .map_err(|error| {
        if is_session_fact_inventory_limit_error(&error) {
            RemoteFactPrepareError::InventoryTooLarge
        } else {
            RemoteFactPrepareError::Internal(error.into())
        }
    })
}

fn serialized_snapshot_fact_bytes(fact: &RemoteUsageEventFact) -> io::Result<usize> {
    let record = FrozenFactRecord::Snapshot(RemoteUsageEventFactRecord {
        event_id: fact.event_id.clone(),
        occurred_at: fact.occurred_at,
        revision: NonZeroU64::MAX,
        mutation: RemoteUsageEventFactMutation::Upsert(Box::new(fact.clone())),
    });
    serde_json::to_vec(&FrozenBatchEntry::Record(record))
        .map(|payload| payload.len())
        .map_err(|error| invalid_data(format!("frozen fact snapshot record is invalid: {error}")))
}

#[derive(Debug)]
pub enum RemoteFactPrepareError {
    Busy,
    FactCursorExpired,
    IncompleteScan,
    DigestChanged,
    InventoryTooLarge,
    Internal(anyhow::Error),
}

impl fmt::Display for RemoteFactPrepareError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Busy => formatter.write_str("remote fact exporter is already running"),
            Self::FactCursorExpired => formatter.write_str("remote fact cursor or batch expired"),
            Self::IncompleteScan => {
                formatter.write_str("remote fact inventory requires a complete rollout scan")
            }
            Self::DigestChanged => {
                formatter.write_str("remote fact digest changed after center planning")
            }
            Self::InventoryTooLarge => formatter
                .write_str("remote fact inventory exceeds the bounded complete-batch limit"),
            Self::Internal(_) => formatter.write_str("remote fact export failed"),
        }
    }
}

impl std::error::Error for RemoteFactPrepareError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Internal(error) => Some(error.as_ref()),
            Self::Busy
            | Self::FactCursorExpired
            | Self::IncompleteScan
            | Self::DigestChanged
            | Self::InventoryTooLarge => None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct PreparedRemoteFactPage {
    observed_at: DateTime<Utc>,
    header: FrozenBatchHeader,
    batch_cursor_generation: NonZeroU64,
    start_sequence: u64,
    batch_has_more: bool,
    records: Vec<FrozenFactRecord>,
}

impl PreparedRemoteFactPage {
    pub fn observed_at(&self) -> DateTime<Utc> {
        self.observed_at
    }

    pub fn entry_count(&self) -> usize {
        self.records.len()
    }

    /// Largest deterministic prefix whose tagged frozen JSON stays within
    /// the conservative wire-page budget. A complete batch is at most 8 MiB
    /// and each record is at most 64 KiB, so it needs at most five data pages.
    pub(crate) fn safe_wire_entry_limit(&self) -> io::Result<usize> {
        let mut entries = 0usize;
        let mut serialized_bytes = 0usize;
        for record in &self.records {
            let record_bytes = serde_json::to_vec(&FrozenBatchEntry::Record(record.clone()))
                .map_err(|error| invalid_data(format!("frozen fact record is invalid: {error}")))?
                .len();
            if record_bytes > MAX_FROZEN_FACT_RECORD_SERIALIZED_BYTES {
                return Err(invalid_data(
                    "frozen fact record exceeds its complete-batch wire bound",
                ));
            }
            let next = serialized_bytes
                .checked_add(record_bytes)
                .and_then(|total| total.checked_add(usize::from(entries > 0)))
                .ok_or_else(|| invalid_data("frozen fact page size overflowed"))?;
            if entries > 0 && next > MAX_SAFE_FACT_PAGE_SERIALIZED_BYTES {
                break;
            }
            serialized_bytes = next;
            entries += 1;
        }
        Ok(entries)
    }

    pub fn decode(
        &self,
    ) -> Result<(PreparedRemoteFactPageEnvelope, RemoteSessionFactPayload), io::Error> {
        self.decode_prefix(self.records.len())
    }

    pub fn decode_prefix(
        &self,
        entry_limit: usize,
    ) -> Result<(PreparedRemoteFactPageEnvelope, RemoteSessionFactPayload), io::Error> {
        if self.records.is_empty() {
            if entry_limit != 0 {
                return Err(invalid_data(
                    "empty remote fact page requires a zero entry limit",
                ));
            }
        } else if entry_limit == 0 || entry_limit > self.records.len() {
            return Err(invalid_data(
                "remote fact page prefix is outside its durable bounds",
            ));
        }
        let emitted = &self.records[..entry_limit];
        let emitted_count = u64::try_from(entry_limit).map_err(integer_error)?;
        let through_batch_sequence = self
            .start_sequence
            .checked_add(emitted_count)
            .ok_or_else(|| invalid_data("remote fact batch sequence overflowed"))?;
        let has_more = self.batch_has_more || entry_limit < self.records.len();
        let activation = (!has_more).then_some(FactCursor {
            fact_generation: self.header.fact_generation,
            through_sequence: self.header.watermark,
        });

        match (&self.header.kind, self.header.batch_identity.as_str()) {
            (FrozenBatchKind::Snapshot, identity) => {
                let snapshot_id = FactSnapshotId::from_str(identity)
                    .map_err(|error| invalid_data(error.to_string()))?;
                let next_page_token = has_more
                    .then(|| {
                        snapshot_page_token(self.batch_cursor_generation, through_batch_sequence)
                    })
                    .transpose()?;
                let records = emitted
                    .iter()
                    .map(|record| match record {
                        FrozenFactRecord::Snapshot(record) => Ok(record.clone()),
                        FrozenFactRecord::Delta(_) => Err(invalid_data(
                            "frozen snapshot batch contains a delta record",
                        )),
                    })
                    .collect::<io::Result<Vec<_>>>()?;
                Ok((
                    PreparedRemoteFactPageEnvelope::Snapshot(FactSnapshotPage {
                        thread_id: self.header.thread_id.clone(),
                        snapshot_id,
                        fact_generation: self.header.fact_generation,
                        snapshot_watermark: self.header.watermark,
                        next_page_token,
                        activate_fact_cursor: activation,
                        has_more,
                    }),
                    RemoteSessionFactPayload::Snapshot(RemoteFactSnapshotPayload {
                        fact_schema_version: REMOTE_SESSION_FACT_SCHEMA_VERSION,
                        records,
                    }),
                ))
            }
            (FrozenBatchKind::Delta, identity) => {
                let batch_id = FactBatchId::from_str(identity)
                    .map_err(|error| invalid_data(error.to_string()))?;
                let next_page_token = has_more
                    .then(|| delta_page_token(self.batch_cursor_generation, through_batch_sequence))
                    .transpose()?;
                let changes = emitted
                    .iter()
                    .map(|record| match record {
                        FrozenFactRecord::Delta(change) => Ok(change.clone()),
                        FrozenFactRecord::Snapshot(_) => Err(invalid_data(
                            "frozen delta batch contains a snapshot record",
                        )),
                    })
                    .collect::<io::Result<Vec<_>>>()?;
                Ok((
                    PreparedRemoteFactPageEnvelope::Delta(FactDeltaPage {
                        thread_id: self.header.thread_id.clone(),
                        batch_id,
                        fact_generation: self.header.fact_generation,
                        delta_watermark: self.header.watermark,
                        next_page_token,
                        activate_fact_cursor: activation,
                        has_more,
                    }),
                    RemoteSessionFactPayload::Delta(RemoteFactDeltaPayload {
                        fact_schema_version: REMOTE_SESSION_FACT_SCHEMA_VERSION,
                        changes,
                    }),
                ))
            }
        }
    }
}

#[derive(Clone, Debug)]
pub enum PreparedRemoteFactPageEnvelope {
    Snapshot(FactSnapshotPage),
    Delta(FactDeltaPage),
}

pub fn prepare_remote_fact_page(
    config: &CollectConfig,
    identity_store: &SourceIdentityStore,
    identity: &SourceIdentity,
    revisions: &ProtocolRevisions,
    redaction_profile: RedactionProfile,
    request: &SessionFactsRequest,
    observed_at: DateTime<Utc>,
) -> Result<PreparedRemoteFactPage, RemoteFactPrepareError> {
    let revision_root = revision_bound_state_root(identity_store, revisions)
        .map_err(RemoteFactPrepareError::Internal)?;
    let _revision_fence =
        try_acquire_revision_fence(&revision_root, RemoteRevisionFenceMode::Shared)
            .map_err(map_state_error)?;
    let request_index_root = fact_index_root(
        identity_store,
        revisions,
        &source_generation(identity),
        &request.thread_id,
        request.retention_days,
    )
    .map_err(RemoteFactPrepareError::Internal)?;
    let _index_fence =
        try_acquire_revision_fence(&request_index_root, RemoteRevisionFenceMode::Shared)
            .map_err(map_state_error)?;
    match &request.position {
        SessionFactsPosition::SnapshotContinue {
            snapshot_id,
            fact_generation,
            snapshot_watermark,
            page_token,
        } => load_frozen_page(
            identity_store,
            identity,
            revisions,
            redaction_profile,
            request,
            observed_at,
            FrozenBatchKind::Snapshot,
            snapshot_id.as_str(),
            *fact_generation,
            *snapshot_watermark,
            page_token.as_str(),
            None,
        ),
        SessionFactsPosition::DeltaContinue {
            fact_cursor,
            batch_id,
            delta_watermark,
            page_token,
        } => load_frozen_page(
            identity_store,
            identity,
            revisions,
            redaction_profile,
            request,
            observed_at,
            FrozenBatchKind::Delta,
            batch_id.as_str(),
            fact_cursor.fact_generation,
            *delta_watermark,
            page_token.as_str(),
            Some(*fact_cursor),
        ),
        SessionFactsPosition::SnapshotStart | SessionFactsPosition::DeltaStart { .. } => {
            prepare_new_frozen_page(
                config,
                identity_store,
                identity,
                revisions,
                redaction_profile,
                request,
                observed_at,
            )
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn prepare_new_frozen_page(
    config: &CollectConfig,
    identity_store: &SourceIdentityStore,
    identity: &SourceIdentity,
    revisions: &ProtocolRevisions,
    redaction_profile: RedactionProfile,
    request: &SessionFactsRequest,
    observed_at: DateTime<Utc>,
) -> Result<PreparedRemoteFactPage, RemoteFactPrepareError> {
    let range = ExportRange {
        from: observed_at
            .checked_sub_days(Days::new(u64::from(request.retention_days)))
            .unwrap_or(DateTime::<Utc>::MIN_UTC),
        to: observed_at,
    };
    let collection = collect_remote_rollouts(config, &range, observed_at, redaction_profile)
        .map_err(RemoteFactPrepareError::Internal)?;
    if !collection.scan_complete {
        return Err(RemoteFactPrepareError::IncompleteScan);
    }
    let publication = collection
        .aggregate_publication()
        .ok_or(RemoteFactPrepareError::IncompleteScan)?;
    let normalized_observation = source_normalized_observation(
        identity,
        &collection.dataset.tasks,
        publication.observation(),
    );
    let digest_evidence = materialize_local_session_digest_evidence(
        &collection.dataset.calls,
        &normalized_observation.half_hour_buckets,
        observed_at,
        true,
    )
    .map_err(|error| RemoteFactPrepareError::Internal(error.into()))?;
    let current_digests =
        finalize_local_session_digests(identity, &digest_evidence, &normalized_observation)
            .map_err(|error| RemoteFactPrepareError::Internal(error.into()))?;
    if !fact_digest_bindings_match(
        &request.thread_id,
        &request.expected_digests,
        &current_digests,
    ) {
        return Err(RemoteFactPrepareError::DigestChanged);
    }
    let materialized = materialize_complete_session_facts_from_normalized_observation(
        &request.thread_id,
        request.retention_days,
        observed_at,
        &normalized_observation,
        &collection.dataset.calls,
        &collection.partial_reasons,
    )?;

    let source = source_generation(identity);
    let state_root = fact_index_root(
        identity_store,
        revisions,
        &source,
        &request.thread_id,
        request.retention_days,
    )
    .map_err(RemoteFactPrepareError::Internal)?;
    let protected_index_root = state_root.clone();
    let state_store = RemoteExportStateStore::new(state_root, source.clone(), redaction_profile);
    let mut session = state_store
        .try_begin(observed_at)
        .map_err(map_state_error)?;
    let desired = desired_fact_records(&materialized.facts, request.retention_days)
        .map_err(|error| RemoteFactPrepareError::Internal(error.into()))?;
    let preflight_cursor =
        match request.position {
            SessionFactsPosition::DeltaStart { fact_cursor } => Some(
                preflight_fact_delta_reconcile(&session, observed_at, &desired, fact_cursor)?,
            ),
            SessionFactsPosition::SnapshotStart => None,
            SessionFactsPosition::SnapshotContinue { .. }
            | SessionFactsPosition::DeltaContinue { .. } => unreachable!("handled above"),
        };
    session
        .reconcile_materialized_records(
            observed_at,
            &desired,
            RemoteExportReconcileMode::Authoritative,
        )
        .map_err(map_state_error)?;
    let status = session.status().map_err(map_state_error)?;
    if preflight_cursor.is_some_and(|cursor| cursor != status.cursor) {
        return Err(RemoteFactPrepareError::Internal(anyhow::anyhow!(
            "remote fact delta reconcile diverged from its write-free preflight"
        )));
    }
    let fact_generation = status.cursor.generation;
    let watermark = status.cursor.sequence;

    let (kind, original_cursor, records) = match request.position {
        SessionFactsPosition::SnapshotStart => {
            let mut records = session
                .materialized_upserts()
                .map_err(map_state_error)?
                .into_iter()
                .map(|upsert| {
                    let mutation = decode_stored_mutation(upsert.change().payload())?;
                    let StoredFactMutation::Upsert(fact) = mutation else {
                        return Err(invalid_data(
                            "materialized remote fact unexpectedly contains a tombstone",
                        ));
                    };
                    Ok(FrozenFactRecord::Snapshot(RemoteUsageEventFactRecord {
                        event_id: fact.event_id.clone(),
                        occurred_at: fact.occurred_at,
                        revision: NonZeroU64::new(upsert.revision()).ok_or_else(|| {
                            invalid_data("materialized remote fact has a zero revision")
                        })?,
                        mutation: RemoteUsageEventFactMutation::Upsert(fact),
                    }))
                })
                .collect::<io::Result<Vec<_>>>()
                .map_err(|error| RemoteFactPrepareError::Internal(error.into()))?;
            records
                .sort_by(|left, right| frozen_snapshot_key(left).cmp(frozen_snapshot_key(right)));
            (FrozenBatchKind::Snapshot, None, records)
        }
        SessionFactsPosition::DeltaStart { fact_cursor } => {
            let records = read_delta_records(&session, fact_cursor, status.cursor)
                .map_err(map_fact_page_error)?;
            (FrozenBatchKind::Delta, Some(fact_cursor), records)
        }
        SessionFactsPosition::SnapshotContinue { .. }
        | SessionFactsPosition::DeltaContinue { .. } => unreachable!("handled above"),
    };
    drop(session);

    let batch_identity = generate_batch_identity(kind)
        .map_err(|error| RemoteFactPrepareError::Internal(error.into()))?;
    let header = FrozenBatchHeader {
        format_version: BATCH_FORMAT_VERSION,
        kind,
        batch_identity,
        source: source.clone(),
        redaction_profile,
        thread_id: request.thread_id.clone(),
        retention_days: request.retention_days,
        expected_digests: request.expected_digests.clone(),
        fact_generation,
        watermark,
        original_cursor,
        record_count: u64::try_from(records.len())
            .map_err(|error| RemoteFactPrepareError::Internal(error.into()))?,
        created_at: observed_at,
    };
    // Frozen batches live outside the per-thread index lock. Serialize every
    // batch read/write and the global sweep with one source-wide maintenance
    // lock, acquired only after the potentially slow rollout scan and index
    // reconciliation have completed.
    let maintenance_store =
        frozen_batch_maintenance_store(identity_store, revisions, &source, redaction_profile)
            .map_err(RemoteFactPrepareError::Internal)?;
    let _maintenance = maintenance_store
        .try_begin(observed_at)
        .map_err(map_state_error)?;
    sweep_frozen_batches(identity_store, revisions).map_err(RemoteFactPrepareError::Internal)?;
    sweep_remote_export_storage(
        identity_store,
        revisions,
        &protected_index_root,
        SystemTime::now(),
    )
    .map_err(RemoteFactPrepareError::Internal)?;
    if records.is_empty() {
        return Ok(PreparedRemoteFactPage {
            observed_at,
            header,
            batch_cursor_generation: fact_generation,
            start_sequence: 1,
            batch_has_more: false,
            records,
        });
    }
    freeze_batch(
        identity_store,
        revisions,
        &source,
        redaction_profile,
        &header,
        &records,
        observed_at,
    )?;
    load_frozen_page_from_header(
        identity_store,
        revisions,
        &source,
        redaction_profile,
        request,
        observed_at,
        header,
        None,
    )
}

fn preflight_fact_delta_reconcile(
    session: &RemoteExportSession<'_>,
    observed_at: DateTime<Utc>,
    desired: &[RemoteExportDesiredRecord],
    fact_cursor: FactCursor,
) -> Result<DeltaCursor, RemoteFactPrepareError> {
    preflight_fact_delta_reconcile_with_limits(
        session,
        observed_at,
        desired,
        fact_cursor,
        COMPLETE_FACT_BATCH_LIMITS,
    )
}

fn preflight_fact_delta_reconcile_with_limits(
    session: &RemoteExportSession<'_>,
    observed_at: DateTime<Utc>,
    desired: &[RemoteExportDesiredRecord],
    fact_cursor: FactCursor,
    limits: CompleteFactBatchLimits,
) -> Result<DeltaCursor, RemoteFactPrepareError> {
    let cursor = DeltaCursor {
        generation: fact_cursor.fact_generation,
        sequence: fact_cursor.through_sequence,
    };
    let preflight = session
        .preflight_reconciled_delta(
            observed_at,
            desired,
            RemoteExportReconcileMode::Authoritative,
            cursor,
            limits.maximum_records,
            limits.maximum_record_serialized_bytes,
            limits.maximum_record_serialized_bytes,
            limits.maximum_serialized_bytes,
            |entry| {
                let record = FrozenFactRecord::Delta(delta_change_from_entry(entry)?);
                serde_json::to_vec(&FrozenBatchEntry::Record(record))
                    .map(|payload| payload.len())
                    .map_err(|error| {
                        invalid_data(format!(
                            "prospective frozen fact delta record is invalid: {error}"
                        ))
                    })
            },
        )
        .map_err(map_state_error)?;
    match preflight {
        RemoteExportReconcileDeltaPreflight::Ready { cursor, .. } => Ok(cursor),
        // Either condition means the requested incremental batch cannot be
        // completed safely. The protocol's typed recovery path is a fresh
        // snapshot; importantly, no authoritative state was written here.
        RemoteExportReconcileDeltaPreflight::CursorExpired(_)
        | RemoteExportReconcileDeltaPreflight::LimitExceeded => {
            Err(RemoteFactPrepareError::FactCursorExpired)
        }
    }
}

fn fact_digest_bindings_match(
    thread_id: &ThreadId,
    expected: &[SessionFactsDigestBinding],
    current: &[SourceSessionDigest],
) -> bool {
    let current = current
        .iter()
        .filter(|digest| digest.replica().thread_id() == thread_id && digest.exact_event_identity())
        .collect::<Vec<_>>();
    current.len() == expected.len()
        && current.iter().zip(expected).all(|(digest, binding)| {
            let metrics = digest.metrics();
            digest.range_start() == binding.range_start
                && digest.range_end() == binding.range_end
                && digest.covered_through() == binding.covered_through
                && digest.coverage_complete() == binding.coverage_complete
                && digest.fingerprint().as_str() == binding.fingerprint.as_str()
                && digest.project_breakdown_fingerprint().as_str()
                    == binding.project_breakdown_fingerprint.as_str()
                && digest.event_count() == binding.event_count
                && metrics.metric_revision == binding.metric_revision.get()
                && metrics.estimator_revision == binding.estimator_revision.get()
                && metrics.project_breakdown_revision == binding.project_breakdown_revision.get()
                && metrics.api_pricing_catalog_revision
                    == binding.api_pricing_catalog_revision.get()
        })
}

#[allow(clippy::too_many_arguments)]
fn load_frozen_page(
    identity_store: &SourceIdentityStore,
    identity: &SourceIdentity,
    revisions: &ProtocolRevisions,
    redaction_profile: RedactionProfile,
    request: &SessionFactsRequest,
    observed_at: DateTime<Utc>,
    expected_kind: FrozenBatchKind,
    batch_identity: &str,
    fact_generation: NonZeroU64,
    watermark: u64,
    page_token: &str,
    original_cursor: Option<FactCursor>,
) -> Result<PreparedRemoteFactPage, RemoteFactPrepareError> {
    let source = source_generation(identity);
    let token = parse_page_token(page_token)?;
    let maintenance_store =
        frozen_batch_maintenance_store(identity_store, revisions, &source, redaction_profile)
            .map_err(RemoteFactPrepareError::Internal)?;
    let _maintenance = maintenance_store
        .try_begin(observed_at)
        .map_err(map_state_error)?;
    let batch_store = batch_store(
        identity_store,
        revisions,
        &source,
        redaction_profile,
        &request.thread_id,
        request.retention_days,
        expected_kind,
        batch_identity,
    )
    .map_err(RemoteFactPrepareError::Internal)?;
    if !batch_store.namespace_directory().exists() {
        return Err(RemoteFactPrepareError::FactCursorExpired);
    }
    let session = batch_store
        .try_begin(observed_at)
        .map_err(map_state_error)?;
    let header = read_batch_header(&session)
        .map_err(|error| RemoteFactPrepareError::Internal(error.into()))?;
    if header.kind != expected_kind
        || header.batch_identity != batch_identity
        || header.source != source
        || header.redaction_profile != redaction_profile
        || header.thread_id != request.thread_id
        || header.retention_days != request.retention_days
        || header.expected_digests != request.expected_digests
        || header.fact_generation != fact_generation
        || header.watermark != watermark
        || header.original_cursor != original_cursor
    {
        return Err(RemoteFactPrepareError::FactCursorExpired);
    }
    load_frozen_page_with_session(session, request, observed_at, header, Some(token))
}

#[allow(clippy::too_many_arguments)]
fn load_frozen_page_from_header(
    identity_store: &SourceIdentityStore,
    revisions: &ProtocolRevisions,
    source: &SourceGeneration,
    redaction_profile: RedactionProfile,
    request: &SessionFactsRequest,
    observed_at: DateTime<Utc>,
    header: FrozenBatchHeader,
    token: Option<BatchPageToken>,
) -> Result<PreparedRemoteFactPage, RemoteFactPrepareError> {
    let store = batch_store(
        identity_store,
        revisions,
        source,
        redaction_profile,
        &request.thread_id,
        request.retention_days,
        header.kind,
        &header.batch_identity,
    )
    .map_err(RemoteFactPrepareError::Internal)?;
    let session = store.try_begin(observed_at).map_err(map_state_error)?;
    load_frozen_page_with_session(session, request, observed_at, header, token)
}

fn load_frozen_page_with_session(
    session: RemoteExportSession<'_>,
    request: &SessionFactsRequest,
    observed_at: DateTime<Utc>,
    header: FrozenBatchHeader,
    token: Option<BatchPageToken>,
) -> Result<PreparedRemoteFactPage, RemoteFactPrepareError> {
    let status = session.status().map_err(map_state_error)?;
    let token = token.unwrap_or(BatchPageToken {
        generation: status.cursor.generation,
        sequence: 1,
    });
    if token.generation != status.cursor.generation || token.sequence < 1 {
        return Err(RemoteFactPrepareError::FactCursorExpired);
    }
    let total_records = status.cursor.sequence.saturating_sub(1);
    if total_records != header.record_count
        || total_records > u64::try_from(MAX_COMPLETE_FACT_BATCH_RECORDS).unwrap_or(u64::MAX)
        || token.sequence > status.cursor.sequence
    {
        return Err(RemoteFactPrepareError::FactCursorExpired);
    }
    // Durable journal pages are an implementation detail, not a network-page
    // boundary. Load every remaining bounded record so the journal's JSON
    // representation cannot force an extra SSH exchange.
    let start_sequence = token.sequence;
    let mut next = DeltaCursor {
        generation: token.generation,
        sequence: token.sequence,
    };
    let mut records = Vec::new();
    while next.sequence < status.cursor.sequence {
        let page = match session
            .read_page(next, MAX_DURABLE_PAGE_ENTRIES, MAX_DURABLE_PAGE_BYTES)
            .map_err(map_state_error)?
        {
            RemoteDeltaPageRead::Page(page) => page,
            RemoteDeltaPageRead::CursorExpired(_) => {
                return Err(RemoteFactPrepareError::FactCursorExpired);
            }
        };
        if page.entries.is_empty() || page.next_cursor.sequence <= next.sequence {
            return Err(RemoteFactPrepareError::FactCursorExpired);
        }
        records.extend(
            page.entries
                .iter()
                .map(decode_frozen_record)
                .collect::<io::Result<Vec<_>>>()
                .map_err(|error| RemoteFactPrepareError::Internal(error.into()))?,
        );
        if records.len() > MAX_COMPLETE_FACT_BATCH_RECORDS {
            return Err(RemoteFactPrepareError::InventoryTooLarge);
        }
        next = page.next_cursor;
    }
    let expected_remaining = status.cursor.sequence.saturating_sub(start_sequence);
    if u64::try_from(records.len()).unwrap_or(u64::MAX) != expected_remaining {
        return Err(RemoteFactPrepareError::FactCursorExpired);
    }
    validate_header_for_request(&header, request)?;
    Ok(PreparedRemoteFactPage {
        observed_at,
        header,
        batch_cursor_generation: status.cursor.generation,
        start_sequence,
        batch_has_more: false,
        records,
    })
}

fn freeze_batch(
    identity_store: &SourceIdentityStore,
    revisions: &ProtocolRevisions,
    source: &SourceGeneration,
    redaction_profile: RedactionProfile,
    header: &FrozenBatchHeader,
    records: &[FrozenFactRecord],
    observed_at: DateTime<Utc>,
) -> Result<(), RemoteFactPrepareError> {
    let store = batch_store(
        identity_store,
        revisions,
        source,
        redaction_profile,
        &header.thread_id,
        header.retention_days,
        header.kind,
        &header.batch_identity,
    )
    .map_err(RemoteFactPrepareError::Internal)?;
    // Serialize and validate the entire frozen batch before publishing its
    // first journal page. This prevents an oversized legal inventory from
    // leaving a partial batch that the bounded center can never activate.
    let changes = frozen_batch_changes(header, records)?;
    let mut session = store.try_begin(observed_at).map_err(map_state_error)?;
    for page in changes.chunks(MAX_DURABLE_PAGE_ENTRIES) {
        session
            .append_changes(observed_at, page)
            .map_err(map_state_error)?;
    }
    let status = session.status().map_err(map_state_error)?;
    if status.cursor.sequence != header.record_count.saturating_add(1) {
        return Err(RemoteFactPrepareError::Internal(anyhow::anyhow!(
            "frozen fact batch did not commit its complete record count"
        )));
    }
    Ok(())
}

fn frozen_batch_changes(
    header: &FrozenBatchHeader,
    records: &[FrozenFactRecord],
) -> Result<Vec<RemoteExportChange>, RemoteFactPrepareError> {
    if records.len() > MAX_COMPLETE_FACT_BATCH_RECORDS {
        return Err(RemoteFactPrepareError::InventoryTooLarge);
    }
    let mut serialized_bytes = 0usize;
    let mut changes = Vec::with_capacity(records.len().saturating_add(1));
    let header_payload = serde_json::to_vec(&FrozenBatchEntry::Header(header.clone()))
        .map_err(|error| RemoteFactPrepareError::Internal(error.into()))?;
    add_serialized_batch_bytes(&mut serialized_bytes, header_payload.len())?;
    changes.push(
        RemoteExportChange::new("header", header_payload)
            .map_err(|error| RemoteFactPrepareError::Internal(error.into()))?,
    );
    for (index, record) in records.iter().enumerate() {
        let payload = serde_json::to_vec(&FrozenBatchEntry::Record(record.clone()))
            .map_err(|error| RemoteFactPrepareError::Internal(error.into()))?;
        if payload.len() > MAX_FROZEN_FACT_RECORD_SERIALIZED_BYTES {
            return Err(RemoteFactPrepareError::InventoryTooLarge);
        }
        add_serialized_batch_bytes(&mut serialized_bytes, payload.len())?;
        changes.push(
            RemoteExportChange::new(format!("entry-{index:020}"), payload)
                .map_err(|error| RemoteFactPrepareError::Internal(error.into()))?,
        );
    }
    Ok(changes)
}

fn add_serialized_batch_bytes(
    current: &mut usize,
    incoming: usize,
) -> Result<(), RemoteFactPrepareError> {
    *current = current
        .checked_add(incoming)
        .ok_or(RemoteFactPrepareError::InventoryTooLarge)?;
    if *current > MAX_COMPLETE_FACT_BATCH_SERIALIZED_BYTES {
        return Err(RemoteFactPrepareError::InventoryTooLarge);
    }
    Ok(())
}

fn read_batch_header(session: &RemoteExportSession<'_>) -> io::Result<FrozenBatchHeader> {
    let status = session.status()?;
    let page = match session.read_page(
        DeltaCursor {
            generation: status.cursor.generation,
            sequence: 0,
        },
        1,
        MAX_DURABLE_PAGE_BYTES,
    )? {
        RemoteDeltaPageRead::Page(page) => page,
        RemoteDeltaPageRead::CursorExpired(_) => {
            return Err(invalid_data("frozen fact batch header expired"));
        }
    };
    let entry = page
        .entries
        .first()
        .ok_or_else(|| invalid_data("frozen fact batch has no header"))?;
    match serde_json::from_slice::<FrozenBatchEntry>(entry.change().payload())
        .map_err(|error| invalid_data(format!("frozen fact batch header is invalid: {error}")))?
    {
        FrozenBatchEntry::Header(header) => {
            header.validate()?;
            Ok(header)
        }
        FrozenBatchEntry::Record(_) => {
            Err(invalid_data("frozen fact batch begins with a data record"))
        }
    }
}

fn read_delta_records(
    session: &RemoteExportSession<'_>,
    cursor: FactCursor,
    current: DeltaCursor,
) -> Result<Vec<FrozenFactRecord>, FactPageReadError> {
    let mut next = DeltaCursor {
        generation: cursor.fact_generation,
        sequence: cursor.through_sequence,
    };
    let mut records = Vec::new();
    loop {
        let page =
            match session.read_page(next, MAX_DURABLE_PAGE_ENTRIES, MAX_DURABLE_PAGE_BYTES)? {
                RemoteDeltaPageRead::Page(page) => page,
                RemoteDeltaPageRead::CursorExpired(_) => return Err(FactPageReadError::Expired),
            };
        for entry in &page.entries {
            if entry.sequence() > current.sequence {
                break;
            }
            records.push(FrozenFactRecord::Delta(delta_change_from_entry(entry)?));
            if records.len() > MAX_COMPLETE_FACT_BATCH_RECORDS {
                return Err(FactPageReadError::TooLarge);
            }
        }
        if page.through_sequence >= current.sequence {
            break;
        }
        if page.entries.is_empty() {
            return Err(FactPageReadError::Io(invalid_data(
                "remote fact delta journal stopped before its watermark",
            )));
        }
        next = page.next_cursor;
    }
    Ok(records)
}

fn delta_change_from_entry(
    entry: &RemoteExportJournalEntry,
) -> io::Result<RemoteUsageEventFactDeltaChange> {
    let sequence = NonZeroU64::new(entry.sequence())
        .ok_or_else(|| invalid_data("remote fact journal contains a zero sequence"))?;
    let mutation = decode_stored_mutation(entry.change().payload())?;
    let record = match mutation {
        StoredFactMutation::Upsert(fact) => RemoteUsageEventFactRecord {
            event_id: fact.event_id.clone(),
            occurred_at: fact.occurred_at,
            revision: sequence,
            mutation: RemoteUsageEventFactMutation::Upsert(fact),
        },
        StoredFactMutation::Tombstone {
            event_id,
            occurred_at,
        } => RemoteUsageEventFactRecord {
            event_id,
            occurred_at,
            revision: sequence,
            mutation: RemoteUsageEventFactMutation::Tombstone,
        },
    };
    Ok(RemoteUsageEventFactDeltaChange { sequence, record })
}

fn desired_fact_records(
    facts: &[RemoteUsageEventFact],
    retention_days: u16,
) -> io::Result<Vec<RemoteExportDesiredRecord>> {
    let mut desired = facts
        .iter()
        .map(|fact| {
            let logical_hash = domain_hash(LOGICAL_KEY_DOMAIN, fact.event_id.as_str().as_bytes());
            let upsert_payload = serde_json::to_vec(&StoredFactMutation::Upsert(Box::new(
                fact.clone(),
            )))
            .map_err(|error| invalid_data(format!("remote fact upsert is invalid: {error}")))?;
            let tombstone_payload = serde_json::to_vec(&StoredFactMutation::Tombstone {
                event_id: fact.event_id.clone(),
                occurred_at: fact.occurred_at,
            })
            .map_err(|error| invalid_data(format!("remote fact tombstone is invalid: {error}")))?;
            let upsert_id = format!(
                "fact-upsert-sha256-v1-{}",
                domain_hash(UPSERT_CHANGE_DOMAIN, &upsert_payload)
            );
            let tombstone_id = format!(
                "fact-tombstone-sha256-v1-{}",
                domain_hash(TOMBSTONE_CHANGE_DOMAIN, &tombstone_payload)
            );
            let expires_at = fact
                .occurred_at
                .checked_add_days(Days::new(u64::from(retention_days)))
                .unwrap_or(DateTime::<Utc>::MAX_UTC);
            RemoteExportDesiredRecord::new(
                format!("fact:{logical_hash}"),
                expires_at,
                RemoteExportChange::new(upsert_id, upsert_payload)?,
                RemoteExportChange::new(tombstone_id, tombstone_payload)?,
            )
        })
        .collect::<io::Result<Vec<_>>>()?;
    desired.sort_by(|left, right| left.logical_key().cmp(right.logical_key()));
    if desired
        .windows(2)
        .any(|records| records[0].logical_key() == records[1].logical_key())
    {
        return Err(invalid_data("remote fact logical-key hash collision"));
    }
    Ok(desired)
}

fn decode_stored_mutation(payload: &[u8]) -> io::Result<StoredFactMutation> {
    serde_json::from_slice(payload)
        .map_err(|error| invalid_data(format!("remote fact journal payload is invalid: {error}")))
}

fn decode_frozen_record(entry: &RemoteExportJournalEntry) -> io::Result<FrozenFactRecord> {
    match serde_json::from_slice::<FrozenBatchEntry>(entry.change().payload())
        .map_err(|error| invalid_data(format!("frozen fact record is invalid: {error}")))?
    {
        FrozenBatchEntry::Record(record) => Ok(record),
        FrozenBatchEntry::Header(_) => Err(invalid_data(
            "frozen fact data page unexpectedly contains a header",
        )),
    }
}

fn validate_header_for_request(
    header: &FrozenBatchHeader,
    request: &SessionFactsRequest,
) -> Result<(), RemoteFactPrepareError> {
    header
        .validate()
        .map_err(|error| RemoteFactPrepareError::Internal(error.into()))?;
    if header.thread_id != request.thread_id
        || header.retention_days != request.retention_days
        || header.expected_digests != request.expected_digests
    {
        return Err(RemoteFactPrepareError::FactCursorExpired);
    }
    match (&header.kind, &request.position) {
        (FrozenBatchKind::Snapshot, SessionFactsPosition::SnapshotStart)
        | (FrozenBatchKind::Snapshot, SessionFactsPosition::SnapshotContinue { .. })
        | (FrozenBatchKind::Delta, SessionFactsPosition::DeltaStart { .. })
        | (FrozenBatchKind::Delta, SessionFactsPosition::DeltaContinue { .. }) => Ok(()),
        _ => Err(RemoteFactPrepareError::FactCursorExpired),
    }
}

fn fact_index_root(
    identity_store: &SourceIdentityStore,
    revisions: &ProtocolRevisions,
    source: &SourceGeneration,
    thread_id: &ThreadId,
    retention_days: u16,
) -> Result<PathBuf, anyhow::Error> {
    let replica = SessionReplicaKey::new(source.node_id.clone(), thread_id.clone());
    Ok(revision_bound_state_root(identity_store, revisions)?
        .join(FACT_INDEX_NAMESPACE)
        .join(ThreadShardKey::from_replica(&replica).as_str())
        .join(format!("retention-{retention_days}")))
}

fn frozen_batch_maintenance_store(
    identity_store: &SourceIdentityStore,
    revisions: &ProtocolRevisions,
    source: &SourceGeneration,
    redaction_profile: RedactionProfile,
) -> Result<RemoteExportStateStore, anyhow::Error> {
    let root = revision_bound_state_root(identity_store, revisions)?
        .join(FACT_BATCH_MAINTENANCE_NAMESPACE);
    Ok(RemoteExportStateStore::new(
        root,
        source.clone(),
        redaction_profile,
    ))
}

fn frozen_batch_namespace_root(
    identity_store: &SourceIdentityStore,
    revisions: &ProtocolRevisions,
) -> Result<PathBuf, anyhow::Error> {
    Ok(revision_bound_state_root(identity_store, revisions)?.join(FACT_BATCH_NAMESPACE))
}

#[allow(clippy::too_many_arguments)]
fn batch_store(
    identity_store: &SourceIdentityStore,
    revisions: &ProtocolRevisions,
    source: &SourceGeneration,
    redaction_profile: RedactionProfile,
    thread_id: &ThreadId,
    retention_days: u16,
    kind: FrozenBatchKind,
    batch_identity: &str,
) -> Result<RemoteExportStateStore, anyhow::Error> {
    validate_batch_identity(batch_identity)?;
    let replica = SessionReplicaKey::new(source.node_id.clone(), thread_id.clone());
    let root = revision_bound_state_root(identity_store, revisions)?
        .join(FACT_BATCH_NAMESPACE)
        .join(ThreadShardKey::from_replica(&replica).as_str())
        .join(format!("retention-{retention_days}"))
        .join(kind.directory_name())
        .join(batch_identity);
    Ok(RemoteExportStateStore::new(
        root,
        source.clone(),
        redaction_profile,
    ))
}

#[cfg(test)]
fn frozen_batch_kind_root(
    identity_store: &SourceIdentityStore,
    revisions: &ProtocolRevisions,
    source: &SourceGeneration,
    thread_id: &ThreadId,
    retention_days: u16,
    kind: FrozenBatchKind,
) -> Result<PathBuf, anyhow::Error> {
    let replica = SessionReplicaKey::new(source.node_id.clone(), thread_id.clone());
    Ok(revision_bound_state_root(identity_store, revisions)?
        .join(FACT_BATCH_NAMESPACE)
        .join(ThreadShardKey::from_replica(&replica).as_str())
        .join(format!("retention-{retention_days}"))
        .join(kind.directory_name()))
}

fn sweep_frozen_batches(
    identity_store: &SourceIdentityStore,
    revisions: &ProtocolRevisions,
) -> Result<(), anyhow::Error> {
    let root = frozen_batch_namespace_root(identity_store, revisions)?;
    sweep_frozen_batches_at(&root, SystemTime::now())
}

fn sweep_remote_export_storage(
    identity_store: &SourceIdentityStore,
    revisions: &ProtocolRevisions,
    protected_index_root: &Path,
    now: SystemTime,
) -> Result<(), anyhow::Error> {
    let batch_root = frozen_batch_namespace_root(identity_store, revisions)?;
    let (active_batch_threads, batch_scan_saturated) = active_frozen_batch_threads(&batch_root)?;
    // If the bounded batch traversal cannot prove that it saw every active
    // continuation, retain every index. A future sweep converges after the
    // frozen-batch maintenance above reduces the namespace.
    if !batch_scan_saturated {
        let index_root =
            revision_bound_state_root(identity_store, revisions)?.join(FACT_INDEX_NAMESPACE);
        sweep_fact_indexes_at(
            &index_root,
            protected_index_root,
            now,
            &active_batch_threads,
            MAX_FACT_INDEX_DIRECTORY_ENTRIES,
            MAX_RETAINED_FACT_INDEXES,
            MAX_FACT_INDEX_DELETIONS_PER_REQUEST,
        )?;
    }
    crate::remote_exporter::sweep_old_revision_state(identity_store, revisions, now)
}

fn active_frozen_batch_threads(root: &Path) -> Result<(HashSet<String>, bool), anyhow::Error> {
    match validate_frozen_batch_directory(root) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok((HashSet::new(), false));
        }
        Err(error) => return Err(error.into()),
    }
    let scan = scan_frozen_batch_tree(root, MAX_FROZEN_BATCH_DIRECTORY_ENTRIES)?;
    let active = scan
        .candidates
        .into_iter()
        .filter(|candidate| !candidate.retired)
        .map(|candidate| candidate.thread_shard)
        .collect();
    Ok((active, scan.saturated))
}

#[derive(Debug)]
struct FactIndexCandidate {
    path: PathBuf,
    thread_shard: String,
    retention_days: u16,
    modified: SystemTime,
    retired: bool,
    has_index_fence: bool,
}

#[derive(Debug)]
struct FactIndexScan {
    candidates: Vec<FactIndexCandidate>,
    structural_paths: Vec<PathBuf>,
    saturated: bool,
}

fn sweep_fact_indexes_at(
    root: &Path,
    protected_index_root: &Path,
    now: SystemTime,
    active_batch_threads: &HashSet<String>,
    maximum_entries: usize,
    maximum_retained: usize,
    maximum_deletions: usize,
) -> Result<(), anyhow::Error> {
    match validate_frozen_batch_directory(root) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    }
    let scan = scan_fact_index_tree(root, maximum_entries)?;
    let mut candidates = scan.candidates;
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
        if deleted == maximum_deletions {
            break;
        }
        if candidate.path == protected_index_root
            || active_batch_threads.contains(&candidate.thread_shard)
        {
            continue;
        }
        let ttl = StdDuration::from_secs(
            u64::from(candidate.retention_days)
                .saturating_mul(24 * 60 * 60)
                .saturating_add(FACT_INDEX_IN_FLIGHT_GRACE.as_secs()),
        );
        let expired = now
            .duration_since(candidate.modified)
            .is_ok_and(|age| age >= ttl);
        let over_capacity = scan.saturated || retained > maximum_retained;
        // Indexes created before the fixed per-index fence may still be used
        // by an older agent. Capacity alone cannot cross that compatibility
        // boundary; require the full retention plus in-flight grace.
        if !candidate.has_index_fence && !expired {
            continue;
        }
        if !candidate.retired && !expired && !over_capacity {
            continue;
        }
        if retire_and_remove_fact_index(&candidate)? {
            if !candidate.retired {
                retained = retained.saturating_sub(1);
            }
            deleted = deleted.saturating_add(1);
        }
    }

    let mut structural_paths = scan.structural_paths;
    structural_paths.sort_by(|left, right| {
        right
            .components()
            .count()
            .cmp(&left.components().count())
            .then_with(|| left.cmp(right))
    });
    for path in structural_paths {
        if deleted == maximum_deletions {
            break;
        }
        match fs::remove_dir(path) {
            Ok(()) => deleted = deleted.saturating_add(1),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) if error.kind() == io::ErrorKind::DirectoryNotEmpty => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn scan_fact_index_tree(
    root: &Path,
    maximum_entries: usize,
) -> Result<FactIndexScan, anyhow::Error> {
    let mut candidates = Vec::<FactIndexCandidate>::new();
    let mut structural_paths = Vec::new();
    let mut saturated = false;
    for (index, entry) in WalkDir::new(root)
        .min_depth(1)
        .max_depth(8)
        .follow_links(false)
        .into_iter()
        .enumerate()
    {
        if index == maximum_entries {
            saturated = true;
            break;
        }
        let entry = entry?;
        let path = entry.path().to_path_buf();
        let metadata = fs::symlink_metadata(&path)?;
        validate_export_storage_entry(&path, &metadata)?;
        match entry.depth() {
            1 => {
                if !metadata.file_type().is_dir() {
                    return Err(
                        invalid_data("remote fact index thread shard is not a directory").into(),
                    );
                }
                let name = entry
                    .file_name()
                    .to_str()
                    .ok_or_else(|| invalid_data("remote fact index thread shard is not UTF-8"))?;
                ThreadShardKey::from_str(name).map_err(|error| {
                    invalid_data(format!(
                        "remote fact index thread shard is invalid: {error}"
                    ))
                })?;
                structural_paths.push(path);
            }
            2 => {
                if !metadata.file_type().is_dir() {
                    return Err(invalid_data(
                        "remote fact index retention path is not a directory",
                    )
                    .into());
                }
                let name = entry
                    .file_name()
                    .to_str()
                    .ok_or_else(|| invalid_data("remote fact index retention path is not UTF-8"))?;
                let (retention_days, retired) = parse_fact_index_retention_name(name)?;
                let thread_shard = path
                    .parent()
                    .and_then(Path::file_name)
                    .and_then(|name| name.to_str())
                    .ok_or_else(|| invalid_data("remote fact index thread shard is invalid"))?
                    .to_owned();
                candidates.push(FactIndexCandidate {
                    path,
                    thread_shard,
                    retention_days,
                    modified: metadata.modified()?,
                    retired,
                    has_index_fence: false,
                });
            }
            _ => {
                let relative = path
                    .strip_prefix(root)
                    .map_err(|_| invalid_data("remote fact index path escaped its namespace"))?;
                let mut components = relative.components();
                let thread = components
                    .next()
                    .ok_or_else(|| invalid_data("remote fact index path has no thread shard"))?;
                let retention = components
                    .next()
                    .ok_or_else(|| invalid_data("remote fact index path has no retention root"))?;
                let candidate_path = root.join(thread.as_os_str()).join(retention.as_os_str());
                let candidate = candidates
                    .iter_mut()
                    .find(|candidate| candidate.path == candidate_path)
                    .ok_or_else(|| invalid_data("remote fact index child has no validated root"))?;
                candidate.modified = candidate.modified.max(metadata.modified()?);
                if entry.depth() == 3
                    && metadata.file_type().is_file()
                    && entry.file_name() == INDEX_FENCE_LOCK_FILE_NAME
                {
                    candidate.has_index_fence = true;
                }
            }
        }
    }
    validate_frozen_batch_directory(root)?;
    Ok(FactIndexScan {
        candidates,
        structural_paths,
        saturated,
    })
}

fn parse_fact_index_retention_name(name: &str) -> io::Result<(u16, bool)> {
    let (name, retired) = match name.strip_prefix(".gc-") {
        Some(name) => (name, true),
        None => (name, false),
    };
    validate_retention_directory_name(name)?;
    let retention_days = name
        .strip_prefix("retention-")
        .and_then(|value| value.parse::<u16>().ok())
        .ok_or_else(|| invalid_data("remote fact index retention path is invalid"))?;
    Ok((retention_days, retired))
}

fn retire_and_remove_fact_index(candidate: &FactIndexCandidate) -> Result<bool, anyhow::Error> {
    let index_fence =
        match try_acquire_revision_fence(&candidate.path, RemoteRevisionFenceMode::Exclusive) {
            Ok(fence) => fence,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(false),
            Err(error) => return Err(error.into()),
        };
    let locks = match crate::remote_exporter::try_lock_exporter_tree(
        &candidate.path,
        MAX_FACT_INDEX_DIRECTORY_ENTRIES,
    )? {
        Some(locks) => locks,
        None => return Ok(false),
    };
    validate_export_storage_tree(&candidate.path, MAX_FACT_INDEX_DIRECTORY_ENTRIES)?;

    #[cfg(windows)]
    {
        drop(locks);
        drop(index_fence);
    }

    let removal_path = if candidate.retired {
        candidate.path.clone()
    } else {
        match retire_fact_index(&candidate.path) {
            Ok(path) => path,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(false),
            Err(error) => return Err(error.into()),
        }
    };
    let result = match fs::remove_dir_all(&removal_path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(true),
        #[cfg(windows)]
        Err(error) if windows_path_is_busy(&error) => Ok(false),
        Err(error) => Err(error.into()),
    };
    #[cfg(not(windows))]
    {
        drop(locks);
        drop(index_fence);
    }
    if result.as_ref().is_ok_and(|removed| *removed) {
        sync_parent_directory(&removal_path)?;
    }
    result
}

fn retire_fact_index(path: &Path) -> io::Result<PathBuf> {
    let parent = path
        .parent()
        .ok_or_else(|| invalid_data("remote fact index has no parent directory"))?;
    validate_frozen_batch_directory(parent)?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| invalid_data("remote fact index retention path is not UTF-8"))?;
    let (_, retired) = parse_fact_index_retention_name(name)?;
    if retired {
        return Ok(path.to_path_buf());
    }
    let retired_path = parent.join(format!(".gc-{name}"));
    match fs::symlink_metadata(&retired_path) {
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "a retired remote fact index already exists",
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    fs::rename(path, &retired_path)?;
    validate_frozen_batch_directory(&retired_path)?;
    validate_frozen_batch_directory(parent)?;
    sync_parent_directory(&retired_path)?;
    Ok(retired_path)
}

fn validate_export_storage_tree(root: &Path, maximum_entries: usize) -> io::Result<()> {
    validate_frozen_batch_directory(root)?;
    for (index, entry) in WalkDir::new(root)
        .min_depth(1)
        .follow_links(false)
        .into_iter()
        .enumerate()
    {
        if index == maximum_entries {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "remote export storage tree exceeds its validation bound",
            ));
        }
        let entry = entry.map_err(io::Error::other)?;
        let metadata = fs::symlink_metadata(entry.path())?;
        validate_export_storage_entry(entry.path(), &metadata)?;
    }
    validate_frozen_batch_directory(root)
}

fn validate_export_storage_entry(path: &Path, metadata: &fs::Metadata) -> io::Result<()> {
    if metadata.file_type().is_dir() {
        return validate_frozen_batch_directory_metadata(path, metadata);
    }
    if frozen_batch_metadata_is_link_or_reparse(metadata) || !metadata.file_type().is_file() {
        return Err(invalid_data(format!(
            "remote export storage entry is not a private regular file ({})",
            path.display()
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        // SAFETY: geteuid has no preconditions and retains no pointers.
        let effective_uid = unsafe { libc::geteuid() };
        if metadata.uid() != effective_uid || metadata.permissions().mode() & 0o077 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "remote export storage file is not private to the current user",
            ));
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;

        let file = OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)?;
        crate::source_identity::validate_windows_private_file(
            path,
            &file,
            "remote export storage file",
        )?;
    }
    Ok(())
}

#[cfg(windows)]
fn windows_path_is_busy(error: &io::Error) -> bool {
    matches!(error.raw_os_error(), Some(5 | 32 | 33))
}

#[cfg(unix)]
fn sync_parent_directory(path: &Path) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| invalid_data("remote export storage path has no parent"))?;
    File::open(parent)?.sync_all()
}

#[cfg(not(unix))]
fn sync_parent_directory(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[derive(Debug)]
struct FrozenBatchCandidate {
    kind: FrozenBatchKind,
    thread_shard: String,
    modified: SystemTime,
    path: PathBuf,
    retired: bool,
}

fn sweep_frozen_batches_at(root: &Path, now: SystemTime) -> Result<(), anyhow::Error> {
    sweep_frozen_batches_at_with_limit(root, now, MAX_FROZEN_BATCH_DIRECTORY_ENTRIES)
}

fn sweep_frozen_batches_at_with_limit(
    root: &Path,
    now: SystemTime,
    maximum_entries: usize,
) -> Result<(), anyhow::Error> {
    match validate_frozen_batch_directory(root) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    }

    let scan = scan_frozen_batch_tree(root, maximum_entries)?;
    let mut candidates = scan.candidates;

    candidates
        .sort_by(|left, right| (left.modified, &left.path).cmp(&(right.modified, &right.path)));
    let mut retained_snapshots = candidates
        .iter()
        .filter(|candidate| !candidate.retired && candidate.kind == FrozenBatchKind::Snapshot)
        .count();
    let mut retained_deltas = candidates
        .iter()
        .filter(|candidate| !candidate.retired && candidate.kind == FrozenBatchKind::Delta)
        .count();
    let mut deleted = 0_usize;
    for candidate in candidates {
        let retained = match candidate.kind {
            FrozenBatchKind::Snapshot => &mut retained_snapshots,
            FrozenBatchKind::Delta => &mut retained_deltas,
        };
        let expired = now
            .duration_since(candidate.modified)
            .is_ok_and(|age| age >= FROZEN_BATCH_TTL);
        // A saturated traversal must make forward progress instead of failing
        // forever at the same prefix. Under capacity pressure an in-flight
        // batch can therefore receive the protocol's typed cursor-expired
        // recovery, matching the existing per-kind retention behavior.
        if !candidate.retired
            && !scan.saturated
            && !expired
            // Reserve one slot for the new non-empty batch prepared by the
            // caller. Empty starts can conservatively leave one slot unused.
            && *retained < MAX_RETAINED_FROZEN_BATCHES_PER_KIND
        {
            continue;
        }
        if deleted == MAX_FROZEN_BATCH_DELETIONS_PER_REQUEST {
            break;
        }
        // The caller holds the source-wide frozen-batch maintenance lock, so
        // no cooperating reader can be inside this batch. Rename first: after
        // a crash readers see either the complete original path or no batch,
        // while a later sweep recognizes and finishes a retired directory.
        let removal_path = if candidate.retired {
            candidate.path.clone()
        } else {
            retire_frozen_batch(&candidate.path)?
        };
        match fs::remove_dir_all(&removal_path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        if !candidate.retired {
            *retained = retained.saturating_sub(1);
        }
        deleted = deleted.saturating_add(1);
    }

    // Interrupted deletion and the old per-thread sweep can leave empty kind,
    // retention, or thread directories. Remove a bounded number deepest-first
    // so a namespace already above the scan cap converges back below it.
    let mut structural_paths = scan.structural_paths;
    structural_paths.sort_by(|left, right| {
        right
            .components()
            .count()
            .cmp(&left.components().count())
            .then_with(|| left.cmp(right))
    });
    for path in structural_paths {
        if deleted == MAX_FROZEN_BATCH_DELETIONS_PER_REQUEST {
            break;
        }
        match fs::remove_dir(path) {
            Ok(()) => deleted = deleted.saturating_add(1),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) if error.kind() == io::ErrorKind::DirectoryNotEmpty => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

#[derive(Debug)]
struct FrozenBatchScan {
    candidates: Vec<FrozenBatchCandidate>,
    structural_paths: Vec<PathBuf>,
    saturated: bool,
}

fn scan_frozen_batch_tree(
    root: &Path,
    maximum_entries: usize,
) -> Result<FrozenBatchScan, anyhow::Error> {
    let mut candidates = Vec::new();
    let mut structural_paths = Vec::new();
    let mut saturated = false;
    for (index, entry) in WalkDir::new(root)
        .min_depth(1)
        .max_depth(4)
        .follow_links(false)
        .into_iter()
        .enumerate()
    {
        if index == maximum_entries {
            saturated = true;
            break;
        }
        let entry = entry?;
        let path = entry.path().to_path_buf();
        let metadata = fs::symlink_metadata(&path)?;
        validate_frozen_batch_directory_metadata(&path, &metadata)?;
        let name = entry
            .file_name()
            .to_str()
            .ok_or_else(|| invalid_data("remote fact batch path is not valid UTF-8"))?;
        match entry.depth() {
            1 => {
                ThreadShardKey::from_str(name).map_err(|error| {
                    invalid_data(format!("remote fact thread shard is invalid: {error}"))
                })?;
                structural_paths.push(path);
            }
            2 => {
                validate_retention_directory_name(name)?;
                structural_paths.push(path);
            }
            3 => {
                frozen_batch_kind_from_directory_name(name)?;
                structural_paths.push(path);
            }
            4 => {
                let retired = match name.strip_prefix(".gc-") {
                    Some(identity) => {
                        validate_batch_identity(identity)?;
                        true
                    }
                    None => {
                        validate_batch_identity(name)?;
                        false
                    }
                };
                let kind_name = path
                    .parent()
                    .and_then(Path::file_name)
                    .and_then(|name| name.to_str())
                    .ok_or_else(|| invalid_data("remote fact batch kind path is invalid"))?;
                let thread_shard = path
                    .strip_prefix(root)
                    .ok()
                    .and_then(|relative| relative.components().next())
                    .and_then(|component| component.as_os_str().to_str())
                    .ok_or_else(|| invalid_data("remote fact batch thread shard is invalid"))?;
                ThreadShardKey::from_str(thread_shard).map_err(|error| {
                    invalid_data(format!("remote fact thread shard is invalid: {error}"))
                })?;
                candidates.push(FrozenBatchCandidate {
                    kind: frozen_batch_kind_from_directory_name(kind_name)?,
                    thread_shard: thread_shard.to_owned(),
                    modified: metadata.modified()?,
                    path,
                    retired,
                });
            }
            _ => unreachable!("walk depth is constrained to one through four"),
        }
    }
    validate_frozen_batch_directory(root)?;
    Ok(FrozenBatchScan {
        candidates,
        structural_paths,
        saturated,
    })
}

fn retire_frozen_batch(path: &Path) -> io::Result<PathBuf> {
    let parent = path
        .parent()
        .ok_or_else(|| invalid_data("remote fact batch path has no parent"))?;
    validate_frozen_batch_directory(parent)?;
    let identity = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| invalid_data("remote fact batch identity is not valid UTF-8"))?;
    validate_batch_identity(identity).map_err(|error| invalid_data(error.to_string()))?;
    let retired = parent.join(format!(".gc-{identity}"));
    match fs::symlink_metadata(&retired) {
        Ok(_) => {
            return Err(invalid_data(
                "remote fact batch retirement path already exists",
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    fs::rename(path, &retired)?;
    validate_frozen_batch_directory(&retired)?;
    validate_frozen_batch_directory(parent)?;
    Ok(retired)
}

fn validate_retention_directory_name(name: &str) -> io::Result<()> {
    let retention_days = name
        .strip_prefix("retention-")
        .and_then(|value| value.parse::<u16>().ok())
        .filter(|days| *days > 0 && *days <= 35)
        .ok_or_else(|| invalid_data("remote fact batch retention directory is invalid"))?;
    if name != format!("retention-{retention_days}") {
        return Err(invalid_data(
            "remote fact batch retention directory is not canonical",
        ));
    }
    Ok(())
}

fn frozen_batch_kind_from_directory_name(name: &str) -> io::Result<FrozenBatchKind> {
    match name {
        "snapshot" => Ok(FrozenBatchKind::Snapshot),
        "delta" => Ok(FrozenBatchKind::Delta),
        _ => Err(invalid_data("remote fact batch kind directory is invalid")),
    }
}

fn validate_frozen_batch_directory(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    validate_frozen_batch_directory_metadata(path, &metadata)
}

fn validate_frozen_batch_directory_metadata(
    path: &Path,
    metadata: &fs::Metadata,
) -> io::Result<()> {
    if frozen_batch_metadata_is_link_or_reparse(metadata) || !metadata.file_type().is_dir() {
        return Err(invalid_data(format!(
            "remote fact batch path is not a private directory ({})",
            path.display()
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        // SAFETY: geteuid has no preconditions and retains no pointers.
        let effective_uid = unsafe { libc::geteuid() };
        if metadata.uid() != effective_uid {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "remote fact batch directory must be owned by the current user",
            ));
        }
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "remote fact batch directory must not be accessible by group or other users",
            ));
        }
    }
    #[cfg(windows)]
    crate::source_identity::validate_windows_private_directory(
        path,
        "remote fact batch directory",
    )?;
    Ok(())
}

#[cfg(unix)]
fn frozen_batch_metadata_is_link_or_reparse(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(windows)]
fn frozen_batch_metadata_is_link_or_reparse(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(any(unix, windows)))]
fn frozen_batch_metadata_is_link_or_reparse(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

fn source_generation(identity: &SourceIdentity) -> SourceGeneration {
    SourceGeneration {
        node_id: identity.node_id().clone(),
        generation: NonZeroU64::new(identity.generation())
            .expect("validated source identities have a non-zero generation"),
    }
}

fn generate_batch_identity(kind: FrozenBatchKind) -> io::Result<String> {
    let prefix = match kind {
        FrozenBatchKind::Snapshot => "fact-snapshot-",
        FrozenBatchKind::Delta => "fact-delta-",
    };
    for _ in 0..16 {
        let mut random = [0_u8; 16];
        getrandom::fill(&mut random).map_err(|error| {
            io::Error::other(format!(
                "could not generate remote fact batch identity: {error}"
            ))
        })?;
        if random.iter().all(|byte| *byte == 0) {
            continue;
        }
        return Ok(format!("{prefix}{}", lower_hex(&random)));
    }
    Err(io::Error::other(
        "secure random provider repeatedly returned an unusable fact batch identity",
    ))
}

fn validate_batch_identity(identity: &str) -> Result<(), anyhow::Error> {
    if identity.is_empty()
        || identity.len() > 128
        || !identity
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        anyhow::bail!("remote fact batch identity is invalid");
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct BatchPageToken {
    generation: NonZeroU64,
    sequence: u64,
}

fn parse_page_token(value: &str) -> Result<BatchPageToken, RemoteFactPrepareError> {
    let Some(rest) = value.strip_prefix("fact-page-") else {
        return Err(RemoteFactPrepareError::FactCursorExpired);
    };
    let Some((generation, sequence)) = rest.split_once('-') else {
        return Err(RemoteFactPrepareError::FactCursorExpired);
    };
    if generation.len() != 16 || sequence.len() != 16 {
        return Err(RemoteFactPrepareError::FactCursorExpired);
    }
    let generation = u64::from_str_radix(generation, 16)
        .ok()
        .and_then(NonZeroU64::new)
        .ok_or(RemoteFactPrepareError::FactCursorExpired)?;
    let sequence =
        u64::from_str_radix(sequence, 16).map_err(|_| RemoteFactPrepareError::FactCursorExpired)?;
    Ok(BatchPageToken {
        generation,
        sequence,
    })
}

fn page_token_value(generation: NonZeroU64, sequence: u64) -> String {
    format!("fact-page-{:016x}-{sequence:016x}", generation.get())
}

fn snapshot_page_token(generation: NonZeroU64, sequence: u64) -> io::Result<FactSnapshotPageToken> {
    FactSnapshotPageToken::from_str(&page_token_value(generation, sequence))
        .map_err(|error| invalid_data(error.to_string()))
}

fn delta_page_token(generation: NonZeroU64, sequence: u64) -> io::Result<FactDeltaPageToken> {
    FactDeltaPageToken::from_str(&page_token_value(generation, sequence))
        .map_err(|error| invalid_data(error.to_string()))
}

fn domain_hash(domain: &[u8], payload: &[u8]) -> String {
    let mut digest = Sha256::new();
    digest.update(domain);
    digest.update(
        u64::try_from(payload.len())
            .unwrap_or(u64::MAX)
            .to_be_bytes(),
    );
    digest.update(payload);
    lower_hex(&digest.finalize())
}

fn lower_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    output
}

fn frozen_snapshot_key(record: &FrozenFactRecord) -> &str {
    match record {
        FrozenFactRecord::Snapshot(record) => record.event_id.as_str(),
        FrozenFactRecord::Delta(change) => change.record.event_id.as_str(),
    }
}

fn integer_error(error: TryFromIntError) -> io::Error {
    invalid_data(format!("remote fact integer conversion failed: {error}"))
}

fn map_state_error(error: io::Error) -> RemoteFactPrepareError {
    if error.kind() == io::ErrorKind::WouldBlock {
        RemoteFactPrepareError::Busy
    } else {
        RemoteFactPrepareError::Internal(error.into())
    }
}

fn map_fact_page_error(error: FactPageReadError) -> RemoteFactPrepareError {
    match error {
        FactPageReadError::Expired => RemoteFactPrepareError::FactCursorExpired,
        FactPageReadError::TooLarge => RemoteFactPrepareError::InventoryTooLarge,
        FactPageReadError::Io(error) => RemoteFactPrepareError::Internal(error.into()),
    }
}

#[derive(Debug)]
enum FactPageReadError {
    Expired,
    TooLarge,
    Io(io::Error),
}

impl From<io::Error> for FactPageReadError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum FrozenBatchKind {
    Snapshot,
    Delta,
}

impl FrozenBatchKind {
    fn directory_name(self) -> &'static str {
        match self {
            Self::Snapshot => "snapshot",
            Self::Delta => "delta",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct FrozenBatchHeader {
    format_version: u32,
    kind: FrozenBatchKind,
    batch_identity: String,
    source: SourceGeneration,
    redaction_profile: RedactionProfile,
    thread_id: ThreadId,
    retention_days: u16,
    expected_digests: Vec<SessionFactsDigestBinding>,
    fact_generation: NonZeroU64,
    watermark: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    original_cursor: Option<FactCursor>,
    record_count: u64,
    created_at: DateTime<Utc>,
}

impl FrozenBatchHeader {
    fn validate(&self) -> io::Result<()> {
        if self.format_version != BATCH_FORMAT_VERSION
            || self.retention_days == 0
            || self.retention_days > 35
            || self.expected_digests.is_empty()
            || self.expected_digests.len() > 36
            || self
                .expected_digests
                .windows(2)
                .any(|bindings| bindings[0].range_start >= bindings[1].range_start)
            || self.expected_digests.iter().any(|binding| {
                binding.range_end <= binding.range_start
                    || binding.range_end.signed_duration_since(binding.range_start)
                        > chrono::Duration::days(1)
                    || binding.covered_through < binding.range_start
                    || binding.covered_through > binding.range_end
                    || (binding.coverage_complete && binding.covered_through != binding.range_end)
            })
            || self.record_count
                > u64::try_from(MAX_COMPLETE_FACT_BATCH_RECORDS).unwrap_or(u64::MAX)
            || self
                .original_cursor
                .is_some_and(|cursor| cursor.fact_generation != self.fact_generation)
            || matches!(self.kind, FrozenBatchKind::Snapshot) && self.original_cursor.is_some()
            || matches!(self.kind, FrozenBatchKind::Delta) && self.original_cursor.is_none()
        {
            return Err(invalid_data("frozen remote fact batch header is invalid"));
        }
        validate_batch_identity(&self.batch_identity)
            .map_err(|error| invalid_data(error.to_string()))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "batchEntryKind",
    content = "batchEntry",
    rename_all = "camelCase",
    deny_unknown_fields
)]
enum FrozenBatchEntry {
    Header(FrozenBatchHeader),
    Record(FrozenFactRecord),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "recordKind",
    content = "record",
    rename_all = "camelCase",
    deny_unknown_fields
)]
enum FrozenFactRecord {
    Snapshot(RemoteUsageEventFactRecord),
    Delta(RemoteUsageEventFactDeltaChange),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "mutationKind",
    content = "mutation",
    rename_all = "camelCase",
    deny_unknown_fields
)]
enum StoredFactMutation {
    Upsert(Box<RemoteUsageEventFact>),
    Tombstone {
        event_id: UsageEventId,
        occurred_at: DateTime<Utc>,
    },
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use chrono::{Duration, TimeZone};
    use serde_json::json;

    use super::*;
    use crate::remote_agent::current_revisions;

    const THREAD: &str = "01a00000-0000-7000-8000-000000000001";

    fn at(hour: u32, minute: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 30, hour, minute, 0)
            .single()
            .unwrap()
    }

    fn fixture(
        event_count: usize,
    ) -> (
        tempfile::TempDir,
        CollectConfig,
        SourceIdentityStore,
        SourceIdentity,
        DateTime<Utc>,
    ) {
        let directory = tempfile::tempdir().unwrap();
        let codex_home = directory.path().join("codex");
        let project = directory.path().join("private-project-name");
        fs::create_dir_all(&project).unwrap();
        let now = at(12, 0);
        write_rollout(&codex_home, &project, now, event_count);
        let config = CollectConfig {
            codex_home,
            rollout_cache_dir: Some(directory.path().join("cache")),
            ..CollectConfig::default()
        };
        let store =
            SourceIdentityStore::at_path(directory.path().join("state/source-identity.json"));
        let identity = store.load_or_create().unwrap();
        (directory, config, store, identity, now)
    }

    fn write_rollout(codex_home: &Path, project: &Path, now: DateTime<Utc>, event_count: usize) {
        let sessions = codex_home.join("sessions/2026/08/30");
        fs::create_dir_all(&sessions).unwrap();
        let mut records = vec![
            json!({
                "timestamp": (now - Duration::hours(2)).to_rfc3339(),
                "type": "session_meta",
                "payload": {
                    "id": THREAD,
                    "timestamp": (now - Duration::hours(2)).to_rfc3339(),
                    "cwd": project,
                    "user_message": "this must never cross the fact protocol"
                }
            }),
            json!({
                "timestamp": (now - Duration::minutes(110)).to_rfc3339(),
                "type": "event_msg",
                "payload": {"type": "task_started", "turn_id": "turn-1"}
            }),
            json!({
                "timestamp": (now - Duration::minutes(109)).to_rfc3339(),
                "type": "turn_context",
                "payload": {"turn_id": "turn-1", "model": "gpt-5.6-sol"}
            }),
        ];
        for index in 1..=event_count {
            let input = u64::try_from(index).unwrap() * 10;
            let output = u64::try_from(index).unwrap() * 5;
            records.push(json!({
                "timestamp": (now - Duration::minutes(100) + Duration::seconds(index as i64)).to_rfc3339(),
                "type": "event_msg",
                "payload": {"type": "token_count", "info": {
                    "total_token_usage": {
                        "input_tokens": input,
                        "cached_input_tokens": 0,
                        "output_tokens": output,
                        "reasoning_output_tokens": 0,
                        "total_tokens": input + output
                    },
                    "last_token_usage": {
                        "input_tokens": 10,
                        "cached_input_tokens": 0,
                        "output_tokens": 5,
                        "reasoning_output_tokens": 0,
                        "total_tokens": 15
                    }
                }}
            }));
        }
        let contents = records
            .iter()
            .map(serde_json::Value::to_string)
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        fs::write(sessions.join("rollout-facts.jsonl"), contents).unwrap();
    }

    fn expected_fact_digests(
        config: &CollectConfig,
        identity: &SourceIdentity,
        now: DateTime<Utc>,
    ) -> Vec<SessionFactsDigestBinding> {
        let range = ExportRange {
            from: now - Duration::days(35),
            to: now,
        };
        let collection =
            collect_remote_rollouts(config, &range, now, RedactionProfile::Redacted).unwrap();
        let publication = collection.aggregate_publication().unwrap();
        let aggregate = crate::source_export::materialize_source_observation(
            identity,
            RedactionProfile::Redacted,
            &collection.dataset.tasks,
            &collection.dataset.calls,
            publication.observation().clone(),
            true,
        )
        .unwrap();
        aggregate
            .session_digests
            .into_iter()
            .filter(|digest| digest.thread_id.as_str() == THREAD && digest.exact_event_identity)
            .map(|digest| {
                let metrics = digest.metrics;
                SessionFactsDigestBinding {
                    range_start: digest.range_start,
                    range_end: digest.range_end,
                    covered_through: digest.covered_through,
                    coverage_complete: digest.coverage_complete,
                    fingerprint: digest.fingerprint,
                    project_breakdown_fingerprint: digest.project_breakdown_fingerprint,
                    event_count: digest.event_count,
                    metric_revision: metrics.metric_revision,
                    estimator_revision: metrics.estimator_revision,
                    project_breakdown_revision: metrics.project_breakdown_revision,
                    api_pricing_catalog_revision: metrics.api_pricing_catalog_revision,
                }
            })
            .collect()
    }

    fn snapshot_request(
        config: &CollectConfig,
        identity: &SourceIdentity,
        now: DateTime<Utc>,
    ) -> SessionFactsRequest {
        SessionFactsRequest {
            thread_id: ThreadId::from_str(THREAD).unwrap(),
            retention_days: 35,
            expected_digests: expected_fact_digests(config, identity, now),
            position: SessionFactsPosition::SnapshotStart,
        }
    }

    fn create_test_frozen_batch(
        identity_store: &SourceIdentityStore,
        identity: &SourceIdentity,
        thread_id: &ThreadId,
        kind: FrozenBatchKind,
        batch_identity: &str,
        observed_at: DateTime<Utc>,
    ) -> PathBuf {
        let revisions = current_revisions();
        let source = source_generation(identity);
        let store = batch_store(
            identity_store,
            &revisions,
            &source,
            RedactionProfile::Redacted,
            thread_id,
            35,
            kind,
            batch_identity,
        )
        .unwrap();
        drop(store.try_begin(observed_at).unwrap());
        frozen_batch_kind_root(identity_store, &revisions, &source, thread_id, 35, kind)
            .unwrap()
            .join(batch_identity)
    }

    fn future_after_batch_ttl(paths: &[PathBuf]) -> SystemTime {
        paths
            .iter()
            .map(|path| fs::symlink_metadata(path).unwrap().modified().unwrap())
            .max()
            .unwrap()
            .checked_add(FROZEN_BATCH_TTL)
            .and_then(|time| time.checked_add(StdDuration::from_secs(1)))
            .unwrap()
    }

    fn create_test_fact_index(
        identity_store: &SourceIdentityStore,
        identity: &SourceIdentity,
        thread_id: &ThreadId,
        profile: RedactionProfile,
        observed_at: DateTime<Utc>,
    ) -> PathBuf {
        let root = fact_index_root(
            identity_store,
            &current_revisions(),
            &source_generation(identity),
            thread_id,
            35,
        )
        .unwrap();
        drop(try_acquire_revision_fence(&root, RemoteRevisionFenceMode::Shared).unwrap());
        let store = RemoteExportStateStore::new(root.clone(), source_generation(identity), profile);
        drop(store.try_begin(observed_at).unwrap());
        root
    }

    fn future_after_fact_index_ttl(paths: &[PathBuf]) -> SystemTime {
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
            .checked_add(StdDuration::from_secs(35 * 24 * 60 * 60))
            .and_then(|time| time.checked_add(FACT_INDEX_IN_FLIGHT_GRACE))
            .and_then(|time| time.checked_add(StdDuration::from_secs(1)))
            .unwrap()
    }

    #[test]
    fn fact_index_gc_expires_inactive_indexes_but_never_the_current_request() {
        let (_directory, _config, store, identity, now) = fixture(0);
        let current_thread = ThreadId::from_str(THREAD).unwrap();
        let inactive_thread = ThreadId::from_str("01a00000-0000-7000-8000-000000000020").unwrap();
        let current = create_test_fact_index(
            &store,
            &identity,
            &current_thread,
            RedactionProfile::Redacted,
            now,
        );
        let inactive = create_test_fact_index(
            &store,
            &identity,
            &inactive_thread,
            RedactionProfile::Redacted,
            now,
        );
        let future = future_after_fact_index_ttl(&[current.clone(), inactive.clone()]);
        let root = revision_bound_state_root(&store, &current_revisions())
            .unwrap()
            .join(FACT_INDEX_NAMESPACE);

        sweep_fact_indexes_at(
            &root,
            &current,
            future,
            &HashSet::new(),
            MAX_FACT_INDEX_DIRECTORY_ENTRIES,
            MAX_RETAINED_FACT_INDEXES,
            MAX_FACT_INDEX_DELETIONS_PER_REQUEST,
        )
        .unwrap();

        assert!(current.exists());
        assert!(!inactive.exists());
    }

    #[test]
    fn fact_index_gc_skips_an_active_preview_profile_and_never_creates_an_envelope() {
        let (_directory, _config, store, identity, now) = fixture(0);
        let thread = ThreadId::from_str("01a00000-0000-7000-8000-000000000021").unwrap();
        let root = fact_index_root(
            &store,
            &current_revisions(),
            &source_generation(&identity),
            &thread,
            35,
        )
        .unwrap();
        let preview = RemoteExportStateStore::new(
            root.clone(),
            source_generation(&identity),
            RedactionProfile::PreviewEnabled,
        );
        drop(try_acquire_revision_fence(&root, RemoteRevisionFenceMode::Shared).unwrap());
        let active = preview.try_begin(now).unwrap();
        let future = future_after_fact_index_ttl(std::slice::from_ref(&root));
        let namespace = revision_bound_state_root(&store, &current_revisions())
            .unwrap()
            .join(FACT_INDEX_NAMESPACE);

        sweep_fact_indexes_at(
            &namespace,
            &namespace.join("not-current"),
            future,
            &HashSet::new(),
            MAX_FACT_INDEX_DIRECTORY_ENTRIES,
            0,
            MAX_FACT_INDEX_DELETIONS_PER_REQUEST,
        )
        .unwrap();

        assert!(root.exists());
        assert!(
            !RemoteExportStateStore::new(
                root.clone(),
                source_generation(&identity),
                RedactionProfile::Redacted,
            )
            .namespace_directory()
            .exists()
        );
        drop(active);
    }

    #[test]
    fn fact_index_gc_skips_an_active_index_fence() {
        let (_directory, _config, store, identity, now) = fixture(0);
        let thread = ThreadId::from_str("01a00000-0000-7000-8000-000000000029").unwrap();
        let index =
            create_test_fact_index(&store, &identity, &thread, RedactionProfile::Redacted, now);
        let active = try_acquire_revision_fence(&index, RemoteRevisionFenceMode::Shared).unwrap();
        let namespace = revision_bound_state_root(&store, &current_revisions())
            .unwrap()
            .join(FACT_INDEX_NAMESPACE);

        sweep_fact_indexes_at(
            &namespace,
            &namespace.join("not-current"),
            SystemTime::UNIX_EPOCH,
            &HashSet::new(),
            MAX_FACT_INDEX_DIRECTORY_ENTRIES,
            0,
            MAX_FACT_INDEX_DELETIONS_PER_REQUEST,
        )
        .unwrap();

        assert!(index.exists());
        drop(active);
    }

    #[test]
    fn fact_index_gc_capacity_never_bypasses_the_legacy_ttl() {
        let (_directory, _config, store, identity, now) = fixture(0);
        let paths = (30..32)
            .map(|suffix| {
                let thread =
                    ThreadId::from_str(&format!("01a00000-0000-7000-8000-{suffix:012}")).unwrap();
                let root = fact_index_root(
                    &store,
                    &current_revisions(),
                    &source_generation(&identity),
                    &thread,
                    35,
                )
                .unwrap();
                let state = RemoteExportStateStore::new(
                    root.clone(),
                    source_generation(&identity),
                    RedactionProfile::Redacted,
                );
                drop(state.try_begin(now).unwrap());
                assert!(!root.join(INDEX_FENCE_LOCK_FILE_NAME).exists());
                root
            })
            .collect::<Vec<_>>();
        let namespace = revision_bound_state_root(&store, &current_revisions())
            .unwrap()
            .join(FACT_INDEX_NAMESPACE);

        sweep_fact_indexes_at(
            &namespace,
            &namespace.join("not-current"),
            SystemTime::now(),
            &HashSet::new(),
            MAX_FACT_INDEX_DIRECTORY_ENTRIES,
            0,
            MAX_FACT_INDEX_DELETIONS_PER_REQUEST,
        )
        .unwrap();

        assert!(paths.iter().all(|path| path.exists()));
    }

    #[test]
    fn fact_index_gc_protects_threads_with_frozen_continuations() {
        let (_directory, _config, store, identity, now) = fixture(0);
        let thread = ThreadId::from_str("01a00000-0000-7000-8000-000000000022").unwrap();
        let index =
            create_test_fact_index(&store, &identity, &thread, RedactionProfile::Redacted, now);
        let batch = create_test_frozen_batch(
            &store,
            &identity,
            &thread,
            FrozenBatchKind::Snapshot,
            "fact-snapshot-index-protection",
            now,
        );
        let batch_root = frozen_batch_namespace_root(&store, &current_revisions()).unwrap();
        let (active_threads, saturated) = active_frozen_batch_threads(&batch_root).unwrap();
        assert!(!saturated);
        let namespace = revision_bound_state_root(&store, &current_revisions())
            .unwrap()
            .join(FACT_INDEX_NAMESPACE);

        sweep_fact_indexes_at(
            &namespace,
            &namespace.join("not-current"),
            future_after_fact_index_ttl(std::slice::from_ref(&index)),
            &active_threads,
            MAX_FACT_INDEX_DIRECTORY_ENTRIES,
            0,
            MAX_FACT_INDEX_DELETIONS_PER_REQUEST,
        )
        .unwrap();

        assert!(index.exists());
        assert!(batch.exists());
    }

    #[test]
    fn fact_index_gc_finishes_retirement_after_a_crash() {
        let (_directory, _config, store, identity, now) = fixture(0);
        let thread = ThreadId::from_str("01a00000-0000-7000-8000-000000000023").unwrap();
        let index =
            create_test_fact_index(&store, &identity, &thread, RedactionProfile::Redacted, now);
        let retired = retire_fact_index(&index).unwrap();
        let namespace = revision_bound_state_root(&store, &current_revisions())
            .unwrap()
            .join(FACT_INDEX_NAMESPACE);

        sweep_fact_indexes_at(
            &namespace,
            &namespace.join("not-current"),
            SystemTime::UNIX_EPOCH,
            &HashSet::new(),
            MAX_FACT_INDEX_DIRECTORY_ENTRIES,
            MAX_RETAINED_FACT_INDEXES,
            MAX_FACT_INDEX_DELETIONS_PER_REQUEST,
        )
        .unwrap();

        assert!(!retired.exists());
    }

    #[test]
    fn fact_index_gc_deletion_cap_converges() {
        let (_directory, _config, store, identity, now) = fixture(0);
        let paths = (24..28)
            .map(|suffix| {
                let thread =
                    ThreadId::from_str(&format!("01a00000-0000-7000-8000-{suffix:012}")).unwrap();
                create_test_fact_index(&store, &identity, &thread, RedactionProfile::Redacted, now)
            })
            .collect::<Vec<_>>();
        let namespace = revision_bound_state_root(&store, &current_revisions())
            .unwrap()
            .join(FACT_INDEX_NAMESPACE);

        sweep_fact_indexes_at(
            &namespace,
            &namespace.join("not-current"),
            SystemTime::UNIX_EPOCH,
            &HashSet::new(),
            MAX_FACT_INDEX_DIRECTORY_ENTRIES,
            1,
            2,
        )
        .unwrap();
        assert_eq!(paths.iter().filter(|path| path.exists()).count(), 2);
        sweep_fact_indexes_at(
            &namespace,
            &namespace.join("not-current"),
            SystemTime::UNIX_EPOCH,
            &HashSet::new(),
            MAX_FACT_INDEX_DIRECTORY_ENTRIES,
            1,
            2,
        )
        .unwrap();
        assert_eq!(paths.iter().filter(|path| path.exists()).count(), 1);
    }

    #[test]
    fn snapshot_continuation_is_fixed_across_restart_and_broken_rollout() {
        let (directory, config, store, identity, now) = fixture(3);
        let request = snapshot_request(&config, &identity, now);
        let prepared = prepare_remote_fact_page(
            &config,
            &store,
            &identity,
            &current_revisions(),
            RedactionProfile::Redacted,
            &request,
            now,
        )
        .unwrap();
        assert_eq!(prepared.entry_count(), 3);
        let (first_envelope, first_payload) = prepared.decode_prefix(1).unwrap();
        let PreparedRemoteFactPageEnvelope::Snapshot(first_page) = first_envelope else {
            panic!("expected snapshot page");
        };
        assert!(first_page.has_more);
        let RemoteSessionFactPayload::Snapshot(first_payload) = first_payload else {
            panic!("expected snapshot payload");
        };
        assert_eq!(first_payload.records.len(), 1);
        let expected_project = crate::source_model::ObservedProjectKey::from_canonical_path(
            &identity,
            &fs::canonicalize(directory.path().join("private-project-name")).unwrap(),
        )
        .unwrap();
        let RemoteUsageEventFactMutation::Upsert(fact) = &first_payload.records[0].mutation else {
            panic!("snapshot fact must be an upsert");
        };
        assert_eq!(fact.observed_project_key, expected_project);

        let serialized = serde_json::to_string(&first_payload).unwrap();
        assert!(!serialized.contains("this must never cross"));
        assert!(!serialized.contains("private-project-name"));
        assert!(serialized.contains("opk-hmac-sha256-v1-"));

        // Continuation reads stay on their addressed batch and must not pay
        // for (or be coupled to) a global tree scan on every wire page.
        let batch_namespace = frozen_batch_namespace_root(&store, &current_revisions()).unwrap();
        fs::create_dir(batch_namespace.join("unrelated-malformed-entry")).unwrap();

        fs::write(
            directory
                .path()
                .join("codex/sessions/2026/08/30/rollout-facts.jsonl"),
            b"broken later source data\n",
        )
        .unwrap();
        let continuation = SessionFactsRequest {
            thread_id: request.thread_id.clone(),
            retention_days: request.retention_days,
            expected_digests: request.expected_digests.clone(),
            position: SessionFactsPosition::SnapshotContinue {
                snapshot_id: first_page.snapshot_id.clone(),
                fact_generation: first_page.fact_generation,
                snapshot_watermark: first_page.snapshot_watermark,
                page_token: first_page.next_page_token.clone().unwrap(),
            },
        };
        let restarted_store =
            SourceIdentityStore::at_path(directory.path().join("state/source-identity.json"));
        let restarted_identity = restarted_store.load_or_create().unwrap();
        let resumed = prepare_remote_fact_page(
            &config,
            &restarted_store,
            &restarted_identity,
            &current_revisions(),
            RedactionProfile::Redacted,
            &continuation,
            now + Duration::minutes(1),
        )
        .unwrap();
        assert_eq!(resumed.entry_count(), 2);
        let (final_envelope, final_payload) = resumed.decode().unwrap();
        let PreparedRemoteFactPageEnvelope::Snapshot(final_page) = final_envelope else {
            panic!("expected final snapshot page");
        };
        assert!(!final_page.has_more);
        assert_eq!(
            final_page.activate_fact_cursor,
            Some(FactCursor {
                fact_generation: first_page.fact_generation,
                through_sequence: first_page.snapshot_watermark,
            })
        );
        let RemoteSessionFactPayload::Snapshot(final_payload) = final_payload else {
            panic!("expected final snapshot payload");
        };
        assert_eq!(final_payload.records.len(), 2);
    }

    #[test]
    fn global_sweep_expires_batches_from_threads_no_longer_requested() {
        let (_directory, _config, store, identity, now) = fixture(0);
        let current_thread = ThreadId::from_str(THREAD).unwrap();
        let inactive_thread = ThreadId::from_str("01a00000-0000-7000-8000-000000000002").unwrap();
        let current = create_test_frozen_batch(
            &store,
            &identity,
            &current_thread,
            FrozenBatchKind::Snapshot,
            "fact-snapshot-current",
            now,
        );
        let inactive = create_test_frozen_batch(
            &store,
            &identity,
            &inactive_thread,
            FrozenBatchKind::Delta,
            "fact-delta-inactive",
            now,
        );
        let root = frozen_batch_namespace_root(&store, &current_revisions()).unwrap();

        sweep_frozen_batches_at(
            &root,
            future_after_batch_ttl(&[current.clone(), inactive.clone()]),
        )
        .unwrap();

        assert!(!current.exists());
        assert!(!inactive.exists());
        let inactive_replica = SessionReplicaKey::new(identity.node_id().clone(), inactive_thread);
        assert!(
            !root
                .join(ThreadShardKey::from_replica(&inactive_replica).as_str())
                .exists(),
            "empty inactive-thread hierarchy should be pruned"
        );
    }

    #[test]
    fn saturated_global_sweep_deletes_incrementally_instead_of_failing_closed() {
        let (_directory, _config, store, identity, now) = fixture(0);
        let thread = ThreadId::from_str("01a00000-0000-7000-8000-000000000003").unwrap();
        let paths = (0..3)
            .map(|index| {
                create_test_frozen_batch(
                    &store,
                    &identity,
                    &thread,
                    FrozenBatchKind::Snapshot,
                    &format!("fact-snapshot-cap-{index}"),
                    now,
                )
            })
            .collect::<Vec<_>>();
        let root = frozen_batch_namespace_root(&store, &current_revisions()).unwrap();
        let fresh_clock = SystemTime::UNIX_EPOCH;

        // Three structural entries plus the first batch exhaust this injected
        // scan budget. Capacity pressure must evict one scanned batch rather
        // than return an error that permanently prevents future sweeps.
        sweep_frozen_batches_at_with_limit(&root, fresh_clock, 4).unwrap();
        assert_eq!(paths.iter().filter(|path| path.exists()).count(), 2);
        sweep_frozen_batches_at_with_limit(&root, fresh_clock, 4).unwrap();
        assert_eq!(paths.iter().filter(|path| path.exists()).count(), 1);
    }

    #[test]
    fn global_sweep_limits_deletions_per_request_and_converges() {
        let (_directory, _config, store, identity, now) = fixture(0);
        let thread = ThreadId::from_str("01a00000-0000-7000-8000-000000000004").unwrap();
        let paths = (0..(MAX_FROZEN_BATCH_DELETIONS_PER_REQUEST + 2))
            .map(|index| {
                create_test_frozen_batch(
                    &store,
                    &identity,
                    &thread,
                    FrozenBatchKind::Snapshot,
                    &format!("fact-snapshot-expired-{index}"),
                    now,
                )
            })
            .collect::<Vec<_>>();
        let root = frozen_batch_namespace_root(&store, &current_revisions()).unwrap();
        let future = future_after_batch_ttl(&paths);

        sweep_frozen_batches_at(&root, future).unwrap();
        assert_eq!(paths.iter().filter(|path| path.exists()).count(), 2);
        sweep_frozen_batches_at(&root, future).unwrap();
        assert!(paths.iter().all(|path| !path.exists()));
    }

    #[test]
    fn global_sweep_finishes_a_crash_interrupted_retirement() {
        let (_directory, _config, store, identity, now) = fixture(0);
        let thread = ThreadId::from_str("01a00000-0000-7000-8000-000000000006").unwrap();
        let batch = create_test_frozen_batch(
            &store,
            &identity,
            &thread,
            FrozenBatchKind::Snapshot,
            "fact-snapshot-retired",
            now,
        );
        let retired = retire_frozen_batch(&batch).unwrap();
        assert!(!batch.exists());
        assert!(retired.exists());

        let root = frozen_batch_namespace_root(&store, &current_revisions()).unwrap();
        sweep_frozen_batches_at(&root, SystemTime::UNIX_EPOCH).unwrap();

        assert!(!retired.exists());
    }

    #[cfg(unix)]
    #[test]
    fn global_sweep_rejects_symlinked_thread_directory_without_following_it() {
        use std::os::unix::fs::symlink;

        let (directory, _config, store, identity, now) = fixture(0);
        let current_thread = ThreadId::from_str(THREAD).unwrap();
        let current = create_test_frozen_batch(
            &store,
            &identity,
            &current_thread,
            FrozenBatchKind::Snapshot,
            "fact-snapshot-safe",
            now,
        );
        let root = frozen_batch_namespace_root(&store, &current_revisions()).unwrap();
        let outside = directory.path().join("outside");
        fs::create_dir(&outside).unwrap();
        let sentinel = outside.join("sentinel");
        fs::write(&sentinel, b"keep").unwrap();
        let other_thread = ThreadId::from_str("01a00000-0000-7000-8000-000000000005").unwrap();
        let other_replica = SessionReplicaKey::new(identity.node_id().clone(), other_thread);
        symlink(
            &outside,
            root.join(ThreadShardKey::from_replica(&other_replica).as_str()),
        )
        .unwrap();

        assert!(sweep_frozen_batches_at(&root, SystemTime::now()).is_err());
        assert!(current.exists());
        assert_eq!(fs::read(&sentinel).unwrap(), b"keep");
    }

    #[test]
    fn complete_batch_bounds_reject_unfinishable_count_and_bytes() {
        let (_directory, config, store, identity, now) = fixture(1);
        let prepared = prepare_remote_fact_page(
            &config,
            &store,
            &identity,
            &current_revisions(),
            RedactionProfile::Redacted,
            &snapshot_request(&config, &identity, now),
            now,
        )
        .unwrap();

        let mut oversized_header = prepared.header.clone();
        oversized_header.record_count = u64::try_from(MAX_COMPLETE_FACT_BATCH_RECORDS + 1).unwrap();
        assert!(oversized_header.validate().is_err());

        let mut large_record = prepared.records[0].clone();
        let FrozenFactRecord::Snapshot(record) = &mut large_record else {
            panic!("snapshot fixture must freeze a snapshot record");
        };
        let RemoteUsageEventFactMutation::Upsert(fact) = &mut record.mutation else {
            panic!("snapshot fixture must contain an upsert");
        };
        fact.emitting_turn_id = Some("e".repeat(256));
        fact.root_session_turn_id = Some("r".repeat(256));
        fact.model = Some("m".repeat(256));
        fact.service_tier = Some("s".repeat(64));
        let records = vec![large_record; 10_000];
        assert!(matches!(
            frozen_batch_changes(&prepared.header, &records),
            Err(RemoteFactPrepareError::InventoryTooLarge)
        ));
    }

    #[test]
    fn fresh_fact_snapshot_rejects_changed_event_or_project_digest() {
        let (directory, config, store, identity, now) = fixture(1);
        let request = snapshot_request(&config, &identity, now);

        let other_project = directory.path().join("different-private-project");
        fs::create_dir_all(&other_project).unwrap();
        write_rollout(&config.codex_home, &other_project, now, 1);
        assert!(matches!(
            prepare_remote_fact_page(
                &config,
                &store,
                &identity,
                &current_revisions(),
                RedactionProfile::Redacted,
                &request,
                now,
            ),
            Err(RemoteFactPrepareError::DigestChanged)
        ));

        let original_project = directory.path().join("private-project-name");
        write_rollout(&config.codex_home, &original_project, now, 1);
        let rollout = config
            .codex_home
            .join("sessions/2026/08/30/rollout-facts.jsonl");
        let original_timestamp = (now - Duration::minutes(100) + Duration::seconds(1)).to_rfc3339();
        let changed_timestamp = (now - Duration::minutes(100) + Duration::seconds(9)).to_rfc3339();
        let contents = fs::read_to_string(&rollout).unwrap();
        fs::write(
            &rollout,
            contents.replacen(&original_timestamp, &changed_timestamp, 1),
        )
        .unwrap();
        assert!(matches!(
            prepare_remote_fact_page(
                &config,
                &store,
                &identity,
                &current_revisions(),
                RedactionProfile::Redacted,
                &request,
                now,
            ),
            Err(RemoteFactPrepareError::DigestChanged)
        ));
    }

    #[test]
    fn delta_uses_the_snapshot_generation_and_only_emits_new_transitions() {
        let (directory, config, store, identity, now) = fixture(2);
        let snapshot = prepare_remote_fact_page(
            &config,
            &store,
            &identity,
            &current_revisions(),
            RedactionProfile::Redacted,
            &snapshot_request(&config, &identity, now),
            now,
        )
        .unwrap();
        let (snapshot_envelope, _) = snapshot.decode().unwrap();
        let PreparedRemoteFactPageEnvelope::Snapshot(snapshot_page) = snapshot_envelope else {
            panic!("expected snapshot page");
        };
        let cursor = snapshot_page.activate_fact_cursor.unwrap();

        let project = directory.path().join("private-project-name");
        write_rollout(&config.codex_home, &project, now, 3);
        let delta_request = SessionFactsRequest {
            thread_id: ThreadId::from_str(THREAD).unwrap(),
            retention_days: 35,
            expected_digests: expected_fact_digests(&config, &identity, now + Duration::minutes(1)),
            position: SessionFactsPosition::DeltaStart {
                fact_cursor: cursor,
            },
        };
        let delta = prepare_remote_fact_page(
            &config,
            &store,
            &identity,
            &current_revisions(),
            RedactionProfile::Redacted,
            &delta_request,
            now + Duration::minutes(1),
        )
        .unwrap();
        let (delta_envelope, delta_payload) = delta.decode().unwrap();
        let PreparedRemoteFactPageEnvelope::Delta(delta_page) = delta_envelope else {
            panic!("expected delta page");
        };
        let RemoteSessionFactPayload::Delta(delta_payload) = delta_payload else {
            panic!("expected delta payload");
        };
        assert_eq!(delta_page.fact_generation, cursor.fact_generation);
        assert_eq!(delta_payload.changes.len(), 1);
        assert!(delta_payload.changes[0].sequence.get() > cursor.through_sequence);
        assert_eq!(
            delta_payload.changes[0].sequence,
            delta_payload.changes[0].record.revision
        );

        let wrong_generation = SessionFactsRequest {
            position: SessionFactsPosition::DeltaStart {
                fact_cursor: FactCursor {
                    fact_generation: NonZeroU64::new(cursor.fact_generation.get() ^ 1).unwrap(),
                    through_sequence: cursor.through_sequence,
                },
            },
            ..delta_request
        };
        assert!(matches!(
            prepare_remote_fact_page(
                &config,
                &store,
                &identity,
                &current_revisions(),
                RedactionProfile::Redacted,
                &wrong_generation,
                now + Duration::minutes(2),
            ),
            Err(RemoteFactPrepareError::FactCursorExpired)
        ));
    }

    #[test]
    fn oversized_authoritative_delta_expires_cursor_without_mutating_index() {
        let (_directory, config, store, identity, now) = fixture(2);
        let request = snapshot_request(&config, &identity, now);
        let snapshot = prepare_remote_fact_page(
            &config,
            &store,
            &identity,
            &current_revisions(),
            RedactionProfile::Redacted,
            &request,
            now,
        )
        .unwrap();
        let (snapshot_envelope, _) = snapshot.decode().unwrap();
        let PreparedRemoteFactPageEnvelope::Snapshot(snapshot_page) = snapshot_envelope else {
            panic!("expected snapshot page");
        };
        let fact_cursor = snapshot_page.activate_fact_cursor.unwrap();

        // Replace every logical event. The authoritative delta is therefore
        // two tombstones plus two upserts, although either complete snapshot
        // contains only two records.
        let replacement_facts = snapshot
            .records
            .iter()
            .enumerate()
            .map(|(index, record)| {
                let FrozenFactRecord::Snapshot(record) = record else {
                    panic!("snapshot preparation must contain snapshot records");
                };
                let RemoteUsageEventFactMutation::Upsert(fact) = &record.mutation else {
                    panic!("snapshot preparation must contain upserts");
                };
                let mut fact = fact.as_ref().clone();
                fact.event_id =
                    UsageEventId::from_str(&format!("usage-replacement-sha256-v1-{index:064x}"))
                        .unwrap();
                fact
            })
            .collect::<Vec<_>>();
        validate_complete_fact_inventory(&replacement_facts).unwrap();
        let desired = desired_fact_records(&replacement_facts, request.retention_days).unwrap();

        let source = source_generation(&identity);
        let index = RemoteExportStateStore::new(
            fact_index_root(
                &store,
                &current_revisions(),
                &source,
                &request.thread_id,
                request.retention_days,
            )
            .unwrap(),
            source,
            RedactionProfile::Redacted,
        );
        let mut session = index.try_begin(now + Duration::minutes(1)).unwrap();
        let status_before = session.status().unwrap();
        let upserts_before = session.materialized_upserts().unwrap();

        let result = preflight_fact_delta_reconcile_with_limits(
            &session,
            now + Duration::minutes(1),
            &desired,
            fact_cursor,
            CompleteFactBatchLimits {
                maximum_records: 3,
                ..COMPLETE_FACT_BATCH_LIMITS
            },
        );
        assert!(matches!(
            result,
            Err(RemoteFactPrepareError::FactCursorExpired)
        ));
        assert_eq!(session.status().unwrap(), status_before);
        assert_eq!(session.materialized_upserts().unwrap(), upserts_before);

        let byte_limited = preflight_fact_delta_reconcile_with_limits(
            &session,
            now + Duration::minutes(1),
            &desired,
            fact_cursor,
            CompleteFactBatchLimits {
                maximum_records: COMPLETE_FACT_BATCH_LIMITS.maximum_records,
                maximum_serialized_bytes: MAX_FROZEN_FACT_RECORD_SERIALIZED_BYTES,
                ..COMPLETE_FACT_BATCH_LIMITS
            },
        );
        assert!(matches!(
            byte_limited,
            Err(RemoteFactPrepareError::FactCursorExpired)
        ));
        assert_eq!(session.status().unwrap(), status_before);
        assert_eq!(session.materialized_upserts().unwrap(), upserts_before);

        // The typed cursor-expired result directs the center to the snapshot
        // path. That same desired inventory remains valid and can reconcile
        // after the failed delta preflight because no earlier mutation leaked.
        session
            .reconcile_materialized_records(
                now + Duration::minutes(1),
                &desired,
                RemoteExportReconcileMode::Authoritative,
            )
            .unwrap();
        assert_eq!(session.materialized_upserts().unwrap().len(), 2);
        assert_eq!(
            session.status().unwrap().cursor.sequence,
            status_before.cursor.sequence + 4
        );
    }
}
