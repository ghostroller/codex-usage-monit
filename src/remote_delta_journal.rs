//! Typed codec between the durable opaque delta journal and protocol DTOs.
//!
//! This module is intentionally side-effect free. It does not collect rollout
//! data, advance a cursor, or advertise a capability; the durable store owns
//! those operations. The codec makes every journal payload canonical and
//! content-addressed, then revalidates it against the response context before
//! producing wire values. A global journal cursor always emits every scanned
//! transition: request ranges describe collection coverage, not a wire filter.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::io::{self, Write};
use std::num::NonZeroU64;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::remote_export_state::{
    RemoteExportChange, RemoteExportDeltaPage, RemoteExportDesiredRecord, RemoteExportJournalEntry,
};
use crate::remote_protocol::{
    DeltaPage, DeltaPayload, ProtocolRevisions, RemoteDeltaCoverage, RemoteDeltaPayloadContext,
    RemoteDeltaStats, RemoteDeltaWarning, RemoteLiveState, RemotePagePayload,
    RemoteProjectDescriptor, RemoteSessionDigestChange, RemoteSessionDigestMutation,
    RemoteUsageBucketChange, RemoteUsageBucketMutation, SourceGeneration,
};
use crate::source_history::RedactionProfile;

const MAX_JOURNAL_RECORD_BYTES: usize = 1024 * 1024;
const MAX_JOURNAL_PAGE_ENTRIES: usize = 4096;
const MAX_JOURNAL_PAGE_BYTES: usize = 8 * 1024 * 1024;
const MAX_DESCRIPTORS_PER_RECORD: usize = 16_384;
const MAX_DESCRIPTORS_PER_PAGE: usize = 16_384;
const MAX_REPOSITORY_RELATIVE_ROOT_BYTES: usize = 2 * 1024;
const CHANGE_ID_PREFIX: &str = "delta-journal-sha256-v1-";
const BUCKET_MINUTES: i64 = 15;

/// Sequence-free aggregate mutation persisted as one opaque journal entry.
/// The adapter assigns the durable entry sequence to both wire sequence and
/// wire revision, preventing either value from regressing independently.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "recordKind",
    content = "record",
    rename_all = "camelCase",
    deny_unknown_fields
)]
pub enum RemoteDeltaJournalRecord {
    UsageBucket {
        starts_at: DateTime<Utc>,
        mutation: RemoteUsageBucketMutation,
    },
    SessionDigest {
        thread_id: crate::source_model::ThreadId,
        range_start: DateTime<Utc>,
        range_end: DateTime<Utc>,
        changed_at: DateTime<Utc>,
        retention_through: DateTime<Utc>,
        mutation: RemoteSessionDigestMutation,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RemoteDeltaJournalRecordV1 {
    record: RemoteDeltaJournalRecord,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    project_descriptors: Vec<RemoteProjectDescriptor>,
}

/// Explicit on-disk version tag. Future incompatible records must add a new
/// enum variant instead of being silently accepted as v1.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "journalVersion",
    content = "journalEntry",
    rename_all = "snake_case",
    deny_unknown_fields
)]
enum VersionedRemoteDeltaJournalRecord {
    V1(RemoteDeltaJournalRecordV1),
}

/// Caller-owned page data which is not part of the durable aggregate journal.
/// Derived emitted/scanned counters in stats are overwritten by the adapter;
/// collection diagnostics remain untouched.
#[derive(Clone, Debug)]
pub struct RemoteDeltaJournalPageInputs {
    pub source: SourceGeneration,
    pub redaction_profile: RedactionProfile,
    pub revisions: ProtocolRevisions,
    pub observed_at: DateTime<Utc>,
    pub coverage: RemoteDeltaCoverage,
    pub live: Option<RemoteLiveState>,
    pub stats: RemoteDeltaStats,
    pub warnings: Vec<RemoteDeltaWarning>,
    /// Descriptors needed only by caller-provided live rows. Journal records
    /// already carry an exact descriptor set for their own references.
    pub live_project_descriptors: Vec<RemoteProjectDescriptor>,
}

/// Typed page-decoding failure used by the framing layer to distinguish
/// durable corruption from a valid prefix that merely cannot coexist in one
/// protocol page. Only page-local descriptor union/conflict limits are
/// splittable; an invalid individual record always remains fatal.
#[derive(Debug)]
pub enum RemoteDeltaPageDecodeError {
    Splittable(io::Error),
    Fatal(io::Error),
}

impl RemoteDeltaPageDecodeError {
    pub fn is_splittable(&self) -> bool {
        matches!(self, Self::Splittable(_))
    }

    fn splittable(error: io::Error) -> Self {
        Self::Splittable(error)
    }

    fn fatal(error: io::Error) -> Self {
        Self::Fatal(error)
    }

    pub(crate) fn fatal_message(message: impl Into<String>) -> Self {
        Self::fatal(io::Error::new(io::ErrorKind::InvalidInput, message.into()))
    }

    fn into_io_error(self) -> io::Error {
        match self {
            Self::Splittable(error) | Self::Fatal(error) => error,
        }
    }
}

impl fmt::Display for RemoteDeltaPageDecodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Splittable(error) | Self::Fatal(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for RemoteDeltaPageDecodeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Splittable(error) | Self::Fatal(error) => Some(error),
        }
    }
}

impl From<io::Error> for RemoteDeltaPageDecodeError {
    fn from(error: io::Error) -> Self {
        Self::fatal(error)
    }
}

/// Canonically encodes one sequence-free mutation for durable append.
///
/// Identical descriptors are deduplicated and sorted. A missing, extra, or
/// conflicting descriptor is rejected before persistence.
pub fn encode_remote_delta_journal_record(
    record: RemoteDeltaJournalRecord,
    project_descriptors: Vec<RemoteProjectDescriptor>,
) -> io::Result<RemoteExportChange> {
    validate_record_shape(&record)?;
    let project_descriptors = canonicalize_descriptors(
        project_descriptors,
        MAX_DESCRIPTORS_PER_RECORD,
        "remote delta journal record",
    )?;
    validate_exact_descriptor_references(&record, &project_descriptors)?;
    let versioned = VersionedRemoteDeltaJournalRecord::V1(RemoteDeltaJournalRecordV1 {
        record,
        project_descriptors,
    });
    let payload = encode_canonical_bounded(&versioned)?;
    RemoteExportChange::new(content_derived_change_id(&payload), payload)
}

/// Revalidates every planned upsert and tombstone against the same contextual
/// protocol rules used for a wire page. Export orchestration must call this
/// for the complete desired set before reconciliation so malformed source
/// values can never enter either the current materialized set or its journal.
pub fn validate_remote_delta_desired_records(
    desired: &[RemoteExportDesiredRecord],
    inputs: &RemoteDeltaJournalPageInputs,
) -> io::Result<()> {
    let sequence = NonZeroU64::MIN;
    for record in desired {
        for change in [record.upsert(), record.tombstone()] {
            let decoded = decode_remote_delta_journal_change(change)?;
            validate_decoded_record(&decoded, sequence, inputs.source.generation, inputs)?;
        }
    }
    Ok(())
}

/// Converts one durable page into protocol page and payload DTOs.
///
/// Every opaque entry is hash-checked, strictly decoded, canonicality-checked,
/// and structurally validated even when its aggregate lies outside the
/// requested coverage range. Every scanned record is emitted because skipping
/// one while advancing the global cursor would lose it for later wider center
/// queries.
pub fn decode_remote_delta_journal_page(
    page: &RemoteExportDeltaPage,
    inputs: RemoteDeltaJournalPageInputs,
) -> io::Result<(DeltaPage, DeltaPayload)> {
    decode_remote_delta_journal_page_classified(page, inputs)
        .map_err(RemoteDeltaPageDecodeError::into_io_error)
}

/// Classified variant used by the exporter runtime. A splittable result means
/// the caller may retry a shorter *contiguous* prefix of the same durable page
/// without reopening state or advancing past any record.
pub fn decode_remote_delta_journal_page_classified(
    page: &RemoteExportDeltaPage,
    inputs: RemoteDeltaJournalPageInputs,
) -> Result<(DeltaPage, DeltaPayload), RemoteDeltaPageDecodeError> {
    decode_remote_delta_journal_page_with_descriptor_limit(page, inputs, MAX_DESCRIPTORS_PER_PAGE)
}

fn decode_remote_delta_journal_page_with_descriptor_limit(
    page: &RemoteExportDeltaPage,
    mut inputs: RemoteDeltaJournalPageInputs,
    descriptor_union_limit: usize,
) -> Result<(DeltaPage, DeltaPayload), RemoteDeltaPageDecodeError> {
    validate_durable_page(page)?;
    if inputs.live_project_descriptors.len() > MAX_DESCRIPTORS_PER_PAGE {
        return Err(
            invalid_data("remote delta live descriptor input exceeds its page bound").into(),
        );
    }

    let mut payload_bytes = 0_usize;
    let mut content_ids = HashMap::<&str, &[u8]>::with_capacity(page.entries.len());
    let mut descriptors = BTreeMap::<String, RemoteProjectDescriptor>::new();
    let mut bucket_changes = Vec::new();
    let mut session_digest_changes = Vec::new();

    for entry in &page.entries {
        payload_bytes = payload_bytes
            .checked_add(entry.change().payload().len())
            .ok_or_else(|| invalid_data("remote delta journal page byte count overflowed"))?;
        if payload_bytes > MAX_JOURNAL_PAGE_BYTES {
            return Err(
                invalid_data("remote delta journal page exceeds its decoded byte bound").into(),
            );
        }
        remember_content_id(
            &mut content_ids,
            entry.change().change_id(),
            entry.change().payload(),
        )?;

        let decoded = decode_remote_delta_journal_entry(entry)?;
        let sequence = NonZeroU64::new(entry.sequence())
            .ok_or_else(|| invalid_data("remote delta journal sequence must be nonzero"))?;
        validate_decoded_record(&decoded, sequence, page.generation, &inputs)?;
        match decoded.record {
            RemoteDeltaJournalRecord::UsageBucket {
                starts_at,
                mutation,
            } => {
                merge_page_descriptors(
                    &mut descriptors,
                    decoded.project_descriptors,
                    "remote delta journal page",
                    descriptor_union_limit,
                    page.entries.len(),
                )?;
                bucket_changes.push(RemoteUsageBucketChange {
                    sequence,
                    starts_at,
                    revision: sequence,
                    mutation,
                });
            }
            RemoteDeltaJournalRecord::SessionDigest {
                thread_id,
                range_start,
                range_end,
                changed_at,
                retention_through,
                mutation,
            } => {
                merge_page_descriptors(
                    &mut descriptors,
                    decoded.project_descriptors,
                    "remote delta journal page",
                    descriptor_union_limit,
                    page.entries.len(),
                )?;
                session_digest_changes.push(RemoteSessionDigestChange {
                    sequence,
                    thread_id,
                    range_start,
                    range_end,
                    changed_at,
                    retention_through,
                    revision: sequence,
                    mutation,
                });
            }
        }
    }

    merge_page_descriptors(
        &mut descriptors,
        canonicalize_descriptors(
            inputs.live_project_descriptors,
            MAX_DESCRIPTORS_PER_PAGE,
            "remote delta live state",
        )?,
        "remote delta page",
        descriptor_union_limit,
        page.entries.len(),
    )?;
    let project_descriptors = descriptors.into_values().collect::<Vec<_>>();
    validate_page_descriptor_references(
        &project_descriptors,
        &bucket_changes,
        &session_digest_changes,
        inputs.live.as_ref(),
    )?;

    let protocol_page = DeltaPage {
        generation: page.generation,
        from_sequence: page.from_sequence,
        through_sequence: page.through_sequence,
        next_delta_cursor: page.next_cursor,
        has_more: page.has_more,
    };
    let scanned = scanned_records(page)?;
    let (live_tasks, live_turns) = inputs
        .live
        .as_ref()
        .and_then(|live| live.snapshot.as_ref())
        .map(|snapshot| (snapshot.tasks.len(), snapshot.turns.len()))
        .unwrap_or_default();
    inputs.stats.journal_records_scanned = scanned;
    inputs.stats.project_descriptors_emitted = project_descriptors.len() as u64;
    inputs.stats.bucket_changes_emitted = bucket_changes.len() as u64;
    inputs.stats.session_digest_changes_emitted = session_digest_changes.len() as u64;
    inputs.stats.live_tasks_emitted = live_tasks as u64;
    inputs.stats.live_turns_emitted = live_turns as u64;

    let payload = DeltaPayload {
        coverage: inputs.coverage,
        project_descriptors,
        bucket_changes,
        session_digest_changes,
        live: inputs.live,
        stats: inputs.stats,
        warnings: inputs.warnings,
    };
    payload
        .validate_remote_delta_payload(&RemoteDeltaPayloadContext {
            page: &protocol_page,
            request: None,
            source: &inputs.source,
            redaction_profile: inputs.redaction_profile,
            revisions: &inputs.revisions,
            observed_at: inputs.observed_at,
        })
        .map_err(|error| {
            RemoteDeltaPageDecodeError::fatal(invalid_data(format!(
                "invalid remote delta journal page: {error}"
            )))
        })?;
    Ok((protocol_page, payload))
}

fn validate_decoded_record(
    decoded: &RemoteDeltaJournalRecordV1,
    sequence: NonZeroU64,
    generation: NonZeroU64,
    inputs: &RemoteDeltaJournalPageInputs,
) -> io::Result<()> {
    let (bucket_changes, session_digest_changes) = match &decoded.record {
        RemoteDeltaJournalRecord::UsageBucket {
            starts_at,
            mutation,
        } => (
            vec![RemoteUsageBucketChange {
                sequence,
                starts_at: *starts_at,
                revision: sequence,
                mutation: mutation.clone(),
            }],
            Vec::new(),
        ),
        RemoteDeltaJournalRecord::SessionDigest {
            thread_id,
            range_start,
            range_end,
            changed_at,
            retention_through,
            mutation,
        } => (
            Vec::new(),
            vec![RemoteSessionDigestChange {
                sequence,
                thread_id: thread_id.clone(),
                range_start: *range_start,
                range_end: *range_end,
                changed_at: *changed_at,
                retention_through: *retention_through,
                revision: sequence,
                mutation: mutation.clone(),
            }],
        ),
    };
    let page = DeltaPage {
        generation,
        from_sequence: sequence.get(),
        through_sequence: sequence.get(),
        next_delta_cursor: crate::remote_protocol::DeltaCursor {
            generation,
            sequence: sequence.get(),
        },
        has_more: false,
    };
    let payload = DeltaPayload {
        coverage: inputs.coverage.clone(),
        project_descriptors: decoded.project_descriptors.clone(),
        stats: RemoteDeltaStats {
            journal_records_scanned: 1,
            project_descriptors_emitted: decoded.project_descriptors.len() as u64,
            bucket_changes_emitted: bucket_changes.len() as u64,
            session_digest_changes_emitted: session_digest_changes.len() as u64,
            ..RemoteDeltaStats::default()
        },
        bucket_changes,
        session_digest_changes,
        live: None,
        warnings: Vec::new(),
    };
    payload
        .validate_remote_delta_payload(&RemoteDeltaPayloadContext {
            page: &page,
            request: None,
            source: &inputs.source,
            redaction_profile: inputs.redaction_profile,
            revisions: &inputs.revisions,
            observed_at: inputs.observed_at,
        })
        .map_err(|error| invalid_data(format!("invalid remote delta journal record: {error}")))
}

fn decode_remote_delta_journal_entry(
    entry: &RemoteExportJournalEntry,
) -> io::Result<RemoteDeltaJournalRecordV1> {
    decode_remote_delta_journal_change(entry.change())
}

fn decode_remote_delta_journal_change(
    change: &RemoteExportChange,
) -> io::Result<RemoteDeltaJournalRecordV1> {
    let payload = change.payload();
    if payload.len() > MAX_JOURNAL_RECORD_BYTES {
        return Err(invalid_data(
            "remote delta journal record exceeds its decoded byte bound",
        ));
    }
    let expected_id = content_derived_change_id(payload);
    if change.change_id() != expected_id {
        return Err(invalid_data(
            "remote delta journal record content hash does not match its change ID",
        ));
    }
    let versioned: VersionedRemoteDeltaJournalRecord = serde_json::from_slice(payload)
        .map_err(|error| invalid_data(format!("invalid remote delta journal record: {error}")))?;
    let canonical = encode_canonical_bounded(&versioned)?;
    if canonical != payload {
        return Err(invalid_data(
            "remote delta journal record is not in canonical encoding",
        ));
    }
    let VersionedRemoteDeltaJournalRecord::V1(decoded) = versioned;
    validate_record_shape(&decoded.record)?;
    if decoded.project_descriptors.len() > MAX_DESCRIPTORS_PER_RECORD {
        return Err(invalid_data(
            "remote delta journal record has too many project descriptors",
        ));
    }
    let canonical_descriptors = canonicalize_descriptors(
        decoded.project_descriptors.clone(),
        MAX_DESCRIPTORS_PER_RECORD,
        "remote delta journal record",
    )?;
    if canonical_descriptors != decoded.project_descriptors {
        return Err(invalid_data(
            "remote delta journal descriptors are not canonical",
        ));
    }
    validate_exact_descriptor_references(&decoded.record, &decoded.project_descriptors)?;
    Ok(decoded)
}

fn validate_record_shape(record: &RemoteDeltaJournalRecord) -> io::Result<()> {
    match record {
        RemoteDeltaJournalRecord::UsageBucket {
            starts_at,
            mutation,
        } => {
            if starts_at.timestamp().rem_euclid(BUCKET_MINUTES * 60) != 0
                || starts_at.timestamp_subsec_nanos() != 0
            {
                return Err(invalid_data(
                    "remote delta journal bucket key is not 15-minute aligned",
                ));
            }
            if let RemoteUsageBucketMutation::Upsert(bucket) = mutation
                && bucket.starts_at != *starts_at
            {
                return Err(invalid_data(
                    "remote delta journal bucket key does not match its payload",
                ));
            }
        }
        RemoteDeltaJournalRecord::SessionDigest {
            thread_id,
            range_start,
            range_end,
            changed_at,
            retention_through,
            mutation,
        } => {
            if range_end <= range_start
                || changed_at < range_start
                || retention_through < range_end
                || retention_through < changed_at
            {
                return Err(invalid_data(
                    "remote delta journal digest bounds are invalid",
                ));
            }
            if let RemoteSessionDigestMutation::Upsert(digest) = mutation
                && (digest.thread_id != *thread_id
                    || digest.range_start != *range_start
                    || digest.range_end != *range_end
                    || digest.covered_through != *changed_at)
            {
                return Err(invalid_data(
                    "remote delta journal digest key does not match its payload",
                ));
            }
        }
    }
    Ok(())
}

fn canonicalize_descriptors(
    descriptors: Vec<RemoteProjectDescriptor>,
    maximum: usize,
    subject: &str,
) -> io::Result<Vec<RemoteProjectDescriptor>> {
    if descriptors.len() > maximum {
        return Err(invalid_data(format!(
            "{subject} has too many project descriptors"
        )));
    }
    let mut canonical = BTreeMap::<String, RemoteProjectDescriptor>::new();
    for descriptor in descriptors {
        validate_descriptor_shape(&descriptor, subject)?;
        let key = descriptor.observed_project_key.as_str().to_owned();
        match canonical.get(&key) {
            Some(existing) if existing == &descriptor => {}
            Some(_) => {
                return Err(invalid_data(format!(
                    "{subject} has conflicting descriptors for one observed project key"
                )));
            }
            None => {
                canonical.insert(key, descriptor);
            }
        }
    }
    Ok(canonical.into_values().collect())
}

fn validate_descriptor_shape(
    descriptor: &RemoteProjectDescriptor,
    subject: &str,
) -> io::Result<()> {
    let Some(root) = descriptor.git_evidence.repository_relative_workspace_root() else {
        return Ok(());
    };
    if root == "." {
        return Ok(());
    }
    if root.is_empty()
        || root.len() > MAX_REPOSITORY_RELATIVE_ROOT_BYTES
        || root.starts_with('/')
        || root.contains('\\')
        || root.chars().any(|character| {
            character.is_control()
                || matches!(
                    character,
                    '\u{061c}'
                        | '\u{200e}'
                        | '\u{200f}'
                        | '\u{2028}'
                        | '\u{2029}'
                        | '\u{202a}'..='\u{202e}'
                        | '\u{2066}'..='\u{2069}'
                )
        })
        || root
            .split('/')
            .any(|component| component.is_empty() || matches!(component, "." | ".."))
    {
        return Err(invalid_data(format!(
            "{subject} contains an invalid repository-relative workspace root"
        )));
    }
    Ok(())
}

fn merge_page_descriptors(
    target: &mut BTreeMap<String, RemoteProjectDescriptor>,
    descriptors: Vec<RemoteProjectDescriptor>,
    subject: &str,
    maximum: usize,
    page_entry_count: usize,
) -> Result<(), RemoteDeltaPageDecodeError> {
    for descriptor in descriptors {
        let key = descriptor.observed_project_key.as_str().to_owned();
        match target.get(&key) {
            Some(existing) if existing == &descriptor => {}
            Some(_) => {
                return Err(page_descriptor_error(
                    page_entry_count,
                    format!("{subject} has conflicting descriptors for one observed project key"),
                ));
            }
            None => {
                target.insert(key, descriptor);
                if target.len() > maximum {
                    return Err(page_descriptor_error(
                        page_entry_count,
                        "remote delta page exceeds its project descriptor bound",
                    ));
                }
            }
        }
    }
    Ok(())
}

fn page_descriptor_error(
    page_entry_count: usize,
    message: impl Into<String>,
) -> RemoteDeltaPageDecodeError {
    let error = invalid_data(message);
    if page_entry_count > 1 {
        RemoteDeltaPageDecodeError::splittable(error)
    } else {
        RemoteDeltaPageDecodeError::fatal(error)
    }
}

fn validate_exact_descriptor_references(
    record: &RemoteDeltaJournalRecord,
    descriptors: &[RemoteProjectDescriptor],
) -> io::Result<()> {
    let referenced = record_project_keys(record);
    let described = descriptors
        .iter()
        .map(|descriptor| descriptor.observed_project_key.as_str().to_owned())
        .collect::<BTreeSet<_>>();
    if referenced != described {
        return Err(invalid_data(
            "remote delta journal descriptors do not exactly match referenced projects",
        ));
    }
    Ok(())
}

fn record_project_keys(record: &RemoteDeltaJournalRecord) -> BTreeSet<String> {
    match record {
        RemoteDeltaJournalRecord::UsageBucket { mutation, .. } => match mutation {
            RemoteUsageBucketMutation::Upsert(bucket) => bucket
                .project_groups
                .iter()
                .filter_map(|group| group.observed_project_key.as_ref())
                .map(|key| key.as_str().to_owned())
                .collect(),
            RemoteUsageBucketMutation::Tombstone => BTreeSet::new(),
        },
        RemoteDeltaJournalRecord::SessionDigest { mutation, .. } => match mutation {
            RemoteSessionDigestMutation::Upsert(digest) => digest
                .observed_project_keys
                .iter()
                .map(|key| key.as_str().to_owned())
                .collect(),
            RemoteSessionDigestMutation::Tombstone => BTreeSet::new(),
        },
    }
}

fn validate_page_descriptor_references(
    descriptors: &[RemoteProjectDescriptor],
    bucket_changes: &[RemoteUsageBucketChange],
    digest_changes: &[RemoteSessionDigestChange],
    live: Option<&RemoteLiveState>,
) -> io::Result<()> {
    let described = descriptors
        .iter()
        .map(|descriptor| descriptor.observed_project_key.as_str().to_owned())
        .collect::<BTreeSet<_>>();
    let mut referenced = BTreeSet::new();
    for bucket in bucket_changes
        .iter()
        .filter_map(|change| match &change.mutation {
            RemoteUsageBucketMutation::Upsert(bucket) => Some(bucket.as_ref()),
            RemoteUsageBucketMutation::Tombstone => None,
        })
    {
        referenced.extend(
            bucket
                .project_groups
                .iter()
                .filter_map(|group| group.observed_project_key.as_ref())
                .map(|key| key.as_str().to_owned()),
        );
    }
    for digest in digest_changes
        .iter()
        .filter_map(|change| match &change.mutation {
            RemoteSessionDigestMutation::Upsert(digest) => Some(digest.as_ref()),
            RemoteSessionDigestMutation::Tombstone => None,
        })
    {
        referenced.extend(
            digest
                .observed_project_keys
                .iter()
                .map(|key| key.as_str().to_owned()),
        );
    }
    if let Some(snapshot) = live.and_then(|live| live.snapshot.as_ref()) {
        referenced.extend(
            snapshot
                .tasks
                .iter()
                .filter_map(|task| task.observed_project_key.as_ref())
                .map(|key| key.as_str().to_owned()),
        );
    }
    if referenced != described {
        return Err(invalid_data(
            "remote delta page descriptors do not exactly match emitted project references",
        ));
    }
    Ok(())
}

fn validate_durable_page(page: &RemoteExportDeltaPage) -> io::Result<()> {
    if page.entries.len() > MAX_JOURNAL_PAGE_ENTRIES {
        return Err(invalid_data(
            "remote delta journal page has too many entries",
        ));
    }
    if page.next_cursor.generation != page.generation
        || page.next_cursor.sequence != page.through_sequence
    {
        return Err(invalid_data(
            "remote delta journal page cursor does not match its watermark",
        ));
    }
    if page.entries.is_empty() {
        if page.from_sequence != page.through_sequence || page.has_more {
            return Err(invalid_data(
                "remote delta no-change page has invalid cursor semantics",
            ));
        }
        return Ok(());
    }
    if page.from_sequence == 0
        || page.entries.first().map(RemoteExportJournalEntry::sequence) != Some(page.from_sequence)
        || page.entries.last().map(RemoteExportJournalEntry::sequence)
            != Some(page.through_sequence)
    {
        return Err(invalid_data(
            "remote delta journal page sequence bounds are invalid",
        ));
    }
    let mut previous = page.from_sequence - 1;
    for entry in &page.entries {
        let expected = previous
            .checked_add(1)
            .ok_or_else(|| invalid_data("remote delta journal page sequence overflows"))?;
        if entry.sequence() != expected {
            return Err(invalid_data(
                "remote delta journal page sequence is not contiguous",
            ));
        }
        previous = entry.sequence();
    }
    Ok(())
}

fn scanned_records(page: &RemoteExportDeltaPage) -> io::Result<u64> {
    if page.entries.is_empty() {
        return Ok(0);
    }
    page.through_sequence
        .checked_sub(page.from_sequence)
        .and_then(|value| value.checked_add(1))
        .ok_or_else(|| invalid_data("remote delta scanned sequence range overflowed"))
}

fn content_derived_change_id(payload: &[u8]) -> String {
    let digest = Sha256::digest(payload);
    let mut output = String::with_capacity(CHANGE_ID_PREFIX.len() + digest.len() * 2);
    output.push_str(CHANGE_ID_PREFIX);
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for byte in digest {
        output.push(HEX[usize::from(byte >> 4)] as char);
        output.push(HEX[usize::from(byte & 0x0f)] as char);
    }
    output
}

fn remember_content_id<'a>(
    content_ids: &mut HashMap<&'a str, &'a [u8]>,
    change_id: &'a str,
    payload: &'a [u8],
) -> io::Result<()> {
    match content_ids.insert(change_id, payload) {
        Some(existing) if existing != payload => Err(invalid_data(
            "remote delta journal contains a content-hash collision",
        )),
        // A real materialized-set ABA transition may emit the same canonical
        // content at a later sequence. Sequence, not content ID, is the wire
        // revision; exact repeated bytes are therefore valid.
        Some(_) => Ok(()),
        None => Ok(()),
    }
}

fn encode_canonical_bounded<T: Serialize>(value: &T) -> io::Result<Vec<u8>> {
    let mut writer = BoundedVecWriter::new(MAX_JOURNAL_RECORD_BYTES);
    serde_json::to_writer(&mut writer, value)
        .map_err(|error| invalid_data(format!("invalid remote delta journal record: {error}")))?;
    Ok(writer.into_inner())
}

struct BoundedVecWriter {
    bytes: Vec<u8>,
    maximum: usize,
}

impl BoundedVecWriter {
    fn new(maximum: usize) -> Self {
        Self {
            bytes: Vec::new(),
            maximum,
        }
    }

    fn into_inner(self) -> Vec<u8> {
        self.bytes
    }
}

impl Write for BoundedVecWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let next = self
            .bytes
            .len()
            .checked_add(buffer.len())
            .ok_or_else(|| invalid_data("remote delta journal encoding size overflowed"))?;
        if next > self.maximum {
            return Err(invalid_data(
                "remote delta journal record exceeds its encoded byte bound",
            ));
        }
        self.bytes.extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
    use std::num::{NonZeroU32, NonZeroU64};

    use chrono::{Duration, TimeZone};
    use tempfile::tempdir;

    use super::*;
    use crate::remote_export_state::{
        RemoteDeltaPageRead, RemoteExportDeltaPage, RemoteExportDesiredRecord,
        RemoteExportReconcileMode, RemoteExportStateStore,
    };
    use crate::remote_protocol::{
        ExportRange, RemoteApiCostAmount, RemoteModelUsageGroup, RemoteProjectUsageGroup,
        RemoteSessionDigest, RemoteSessionDigestFingerprint, RemoteSessionUsageMetrics,
        RemoteTokenUsage, RemoteU128, RemoteUsageBucket,
    };
    use crate::source_model::ObservedProjectKey;

    const NODE: &str = "node-0123456789abcdef0123456789abcdef";

    fn at(hour: u32, minute: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 30, hour, minute, 0)
            .single()
            .unwrap()
    }

    fn nonzero32(value: u32) -> NonZeroU32 {
        NonZeroU32::new(value).unwrap()
    }

    fn source() -> SourceGeneration {
        SourceGeneration {
            node_id: NODE.parse().unwrap(),
            generation: NonZeroU64::new(7).unwrap(),
        }
    }

    fn revisions() -> ProtocolRevisions {
        ProtocolRevisions {
            history_format: nonzero32(2),
            metric: nonzero32(3),
            estimator: nonzero32(4),
            project_breakdown: nonzero32(5),
            api_pricing_catalog: nonzero32(6),
        }
    }

    fn project_key(value: u64) -> ObservedProjectKey {
        format!("opk-hmac-sha256-v1-{value:064x}").parse().unwrap()
    }

    fn descriptor(value: u64) -> RemoteProjectDescriptor {
        RemoteProjectDescriptor {
            observed_project_key: project_key(value),
            display_label: format!("workspace-{value}").parse().unwrap(),
            git_evidence: crate::remote_protocol::RemoteGitRepositoryEvidence::Unavailable,
        }
    }

    fn tokens() -> RemoteTokenUsage {
        RemoteTokenUsage {
            input_tokens: 100,
            cached_input_tokens: 20,
            cache_write_input_tokens: 5,
            output_tokens: 10,
            reasoning_output_tokens: 4,
            total_tokens: 110,
        }
    }

    fn api_cost() -> RemoteApiCostAmount {
        RemoteApiCostAmount {
            minimum_pico_usd: RemoteU128::new(1_000),
            maximum_pico_usd: RemoteU128::new(1_500),
            observed_samples: 1,
            priced_samples: 1,
            observed_tokens: 110,
            priced_tokens: 110,
        }
    }

    fn bucket_record(
        starts_at: DateTime<Utc>,
        key: ObservedProjectKey,
    ) -> RemoteDeltaJournalRecord {
        RemoteDeltaJournalRecord::UsageBucket {
            starts_at,
            mutation: RemoteUsageBucketMutation::Upsert(Box::new(RemoteUsageBucket {
                starts_at,
                ends_at: starts_at + Duration::minutes(BUCKET_MINUTES),
                sampled_at: starts_at + Duration::minutes(2),
                token_usage: tokens(),
                estimated_cost_units: RemoteU128::new(75),
                api_long_context_extra_cost_units: Some(RemoteU128::new(25)),
                long_context_usage_unknown: false,
                api_equivalent_cost: api_cost(),
                call_count: 1,
                metric_revision: nonzero32(3),
                estimator_revision: nonzero32(4),
                project_breakdown_revision: nonzero32(5),
                api_pricing_catalog_revision: nonzero32(6),
                model_groups: Vec::new(),
                project_groups: vec![RemoteProjectUsageGroup {
                    observed_project_key: Some(key),
                    emitting_thread_id: "thread-1".parse().unwrap(),
                    emitting_turn_id: Some("turn-1".to_owned()),
                    parent_thread_id: None,
                    root_session_thread_id: Some("thread-1".parse().unwrap()),
                    root_session_turn_id: Some("turn-1".to_owned()),
                    title_preview: None,
                    message_preview: None,
                    token_usage: tokens(),
                    estimated_cost_units: RemoteU128::new(75),
                    api_long_context_extra_cost_units: Some(RemoteU128::new(25)),
                    api_equivalent_cost: api_cost(),
                    call_count: 1,
                }],
                partial_reasons: Vec::new(),
            })),
        }
    }

    fn digest_record(key: ObservedProjectKey) -> RemoteDeltaJournalRecord {
        let range_start = at(0, 0);
        let range_end = at(1, 15);
        let changed_at = at(1, 2);
        RemoteDeltaJournalRecord::SessionDigest {
            thread_id: "thread-1".parse().unwrap(),
            range_start,
            range_end,
            changed_at,
            retention_through: at(3, 0),
            mutation: RemoteSessionDigestMutation::Upsert(Box::new(RemoteSessionDigest {
                thread_id: "thread-1".parse().unwrap(),
                range_start,
                range_end,
                covered_through: changed_at,
                fingerprint: format!("session-digest-sha256-v1-{:064x}", 2)
                    .parse::<RemoteSessionDigestFingerprint>()
                    .unwrap(),
                project_breakdown_fingerprint: format!("session-digest-sha256-v1-{:064x}", 3)
                    .parse::<RemoteSessionDigestFingerprint>()
                    .unwrap(),
                event_count: 1,
                exact_event_identity: true,
                coverage_complete: false,
                observed_project_keys: vec![key],
                metrics: RemoteSessionUsageMetrics {
                    token_usage: tokens(),
                    estimated_cost_units: RemoteU128::new(75),
                    api_long_context_extra_cost_units: Some(RemoteU128::new(25)),
                    api_equivalent_cost: api_cost(),
                    call_count: 1,
                    metric_revision: nonzero32(3),
                    estimator_revision: nonzero32(4),
                    project_breakdown_revision: nonzero32(5),
                    api_pricing_catalog_revision: nonzero32(6),
                    partial_reasons: vec!["open_session".to_owned()],
                },
            })),
        }
    }

    fn tombstone_bucket(starts_at: DateTime<Utc>) -> RemoteDeltaJournalRecord {
        RemoteDeltaJournalRecord::UsageBucket {
            starts_at,
            mutation: RemoteUsageBucketMutation::Tombstone,
        }
    }

    fn inputs(from: DateTime<Utc>, to: DateTime<Utc>) -> RemoteDeltaJournalPageInputs {
        let requested_range = ExportRange { from, to };
        RemoteDeltaJournalPageInputs {
            source: source(),
            redaction_profile: RedactionProfile::Redacted,
            revisions: revisions(),
            observed_at: at(4, 0),
            coverage: RemoteDeltaCoverage {
                covered_range: Some(requested_range.clone()),
                requested_range,
                range_complete: true,
                partial_reasons: Vec::new(),
            },
            live: None,
            stats: RemoteDeltaStats::default(),
            warnings: Vec::new(),
            live_project_descriptors: Vec::new(),
        }
    }

    fn page(read: RemoteDeltaPageRead) -> RemoteExportDeltaPage {
        match read {
            RemoteDeltaPageRead::Page(page) => page,
            RemoteDeltaPageRead::CursorExpired(expired) => {
                panic!("unexpected cursor expiry: {expired:?}")
            }
        }
    }

    #[test]
    fn records_round_trip_with_deterministic_ids_sorted_descriptors_and_sequence_revisions() {
        let bucket_descriptor = descriptor(2);
        let digest_descriptor = descriptor(1);
        let bucket = encode_remote_delta_journal_record(
            bucket_record(at(1, 0), bucket_descriptor.observed_project_key.clone()),
            vec![bucket_descriptor.clone(), bucket_descriptor.clone()],
        )
        .unwrap();
        let same_bucket = encode_remote_delta_journal_record(
            bucket_record(at(1, 0), bucket_descriptor.observed_project_key.clone()),
            vec![bucket_descriptor.clone()],
        )
        .unwrap();
        assert_eq!(bucket, same_bucket);
        let digest = encode_remote_delta_journal_record(
            digest_record(digest_descriptor.observed_project_key.clone()),
            vec![digest_descriptor.clone()],
        )
        .unwrap();
        assert_ne!(bucket.change_id(), digest.change_id());

        let directory = tempdir().unwrap();
        let store = RemoteExportStateStore::new(
            directory.path().join("state"),
            source(),
            RedactionProfile::Redacted,
        );
        let mut session = store.try_begin(at(2, 0)).unwrap();
        let cursor = session.status().unwrap().cursor;
        session.append_changes(at(2, 0), &[bucket, digest]).unwrap();
        let durable = page(session.read_page(cursor, 16, 1024 * 1024).unwrap());
        let (wire_page, payload) =
            decode_remote_delta_journal_page(&durable, inputs(at(0, 0), at(3, 0))).unwrap();

        assert_eq!(wire_page.next_delta_cursor, durable.next_cursor);
        assert_eq!(
            payload.project_descriptors,
            vec![digest_descriptor, bucket_descriptor]
        );
        assert_eq!(payload.bucket_changes[0].sequence.get(), 1);
        assert_eq!(payload.bucket_changes[0].revision.get(), 1);
        assert_eq!(payload.session_digest_changes[0].sequence.get(), 2);
        assert_eq!(payload.session_digest_changes[0].revision.get(), 2);
        assert_eq!(payload.stats.journal_records_scanned, 2);
        assert_eq!(payload.stats.project_descriptors_emitted, 2);
    }

    #[test]
    fn no_change_page_preserves_cursor_and_emits_no_journal_records() {
        let directory = tempdir().unwrap();
        let store = RemoteExportStateStore::new(
            directory.path().join("state"),
            source(),
            RedactionProfile::Redacted,
        );
        let session = store.try_begin(at(2, 0)).unwrap();
        let cursor = session.status().unwrap().cursor;
        let durable = page(session.read_page(cursor, 16, 1024 * 1024).unwrap());
        let (wire_page, payload) =
            decode_remote_delta_journal_page(&durable, inputs(at(0, 0), at(3, 0))).unwrap();

        assert_eq!(wire_page.from_sequence, cursor.sequence);
        assert_eq!(wire_page.through_sequence, cursor.sequence);
        assert_eq!(wire_page.next_delta_cursor, cursor);
        assert!(!wire_page.has_more);
        assert!(payload.bucket_changes.is_empty());
        assert!(payload.session_digest_changes.is_empty());
        assert_eq!(payload.stats.journal_records_scanned, 0);
    }

    #[test]
    fn multipage_cursor_progress_emits_every_transition_independent_of_coverage_range() {
        let first_descriptor = descriptor(2);
        let last_descriptor = descriptor(1);
        let changes = [
            encode_remote_delta_journal_record(
                bucket_record(at(1, 0), first_descriptor.observed_project_key.clone()),
                vec![first_descriptor.clone()],
            )
            .unwrap(),
            encode_remote_delta_journal_record(tombstone_bucket(at(3, 0)), Vec::new()).unwrap(),
            encode_remote_delta_journal_record(
                digest_record(last_descriptor.observed_project_key.clone()),
                vec![last_descriptor.clone()],
            )
            .unwrap(),
        ];
        let directory = tempdir().unwrap();
        let store = RemoteExportStateStore::new(
            directory.path().join("state"),
            source(),
            RedactionProfile::Redacted,
        );
        let mut session = store.try_begin(at(3, 30)).unwrap();
        let initial = session.status().unwrap().cursor;
        session.append_changes(at(3, 30), &changes).unwrap();

        let first_durable = page(session.read_page(initial, 2, 1024 * 1024).unwrap());
        let first_retry = page(session.read_page(initial, 2, 1024 * 1024).unwrap());
        assert_eq!(first_durable, first_retry);
        let (first_page, first_payload) =
            decode_remote_delta_journal_page(&first_durable, inputs(at(0, 0), at(2, 0))).unwrap();
        assert!(first_page.has_more);
        assert_eq!(first_page.through_sequence, 2);
        assert_eq!(first_payload.stats.journal_records_scanned, 2);
        assert_eq!(first_payload.bucket_changes.len(), 2);
        assert_eq!(first_payload.project_descriptors, vec![first_descriptor]);

        let second_durable = page(
            session
                .read_page(first_page.next_delta_cursor, 2, 1024 * 1024)
                .unwrap(),
        );
        let (second_page, second_payload) =
            decode_remote_delta_journal_page(&second_durable, inputs(at(0, 0), at(2, 0))).unwrap();
        assert!(!second_page.has_more);
        assert_eq!(second_page.through_sequence, 3);
        assert_eq!(second_payload.stats.journal_records_scanned, 1);
        assert_eq!(second_payload.session_digest_changes.len(), 1);
        assert_eq!(second_payload.project_descriptors, vec![last_descriptor]);

        let unchanged = page(
            session
                .read_page(second_page.next_delta_cursor, 2, 1024 * 1024)
                .unwrap(),
        );
        let (unchanged_page, unchanged_payload) =
            decode_remote_delta_journal_page(&unchanged, inputs(at(0, 0), at(2, 0))).unwrap();
        assert_eq!(
            unchanged_page.next_delta_cursor,
            second_page.next_delta_cursor
        );
        assert_eq!(unchanged_payload.stats.journal_records_scanned, 0);
    }

    #[test]
    fn descriptor_metadata_changes_split_at_a_contiguous_cursor_boundary() {
        let first_descriptor = descriptor(1);
        let mut second_descriptor = first_descriptor.clone();
        second_descriptor.display_label = "renamed-workspace".parse().unwrap();
        let changes = [
            encode_remote_delta_journal_record(
                bucket_record(at(1, 0), first_descriptor.observed_project_key.clone()),
                vec![first_descriptor.clone()],
            )
            .unwrap(),
            encode_remote_delta_journal_record(
                bucket_record(at(1, 15), second_descriptor.observed_project_key.clone()),
                vec![second_descriptor.clone()],
            )
            .unwrap(),
        ];
        let directory = tempdir().unwrap();
        let store = RemoteExportStateStore::new(
            directory.path().join("state"),
            source(),
            RedactionProfile::Redacted,
        );
        let mut session = store.try_begin(at(2, 0)).unwrap();
        let initial = session.status().unwrap().cursor;
        session.append_changes(at(2, 0), &changes).unwrap();

        let combined = page(session.read_page(initial, 16, 1024 * 1024).unwrap());
        let error =
            decode_remote_delta_journal_page_classified(&combined, inputs(at(0, 0), at(3, 0)))
                .unwrap_err();
        assert!(error.is_splittable());

        let first = page(session.read_page(initial, 1, 1024 * 1024).unwrap());
        let (first_page, first_payload) =
            decode_remote_delta_journal_page_classified(&first, inputs(at(0, 0), at(3, 0)))
                .unwrap();
        assert!(first_page.has_more);
        assert_eq!(first_page.through_sequence, 1);
        assert_eq!(first_payload.project_descriptors, vec![first_descriptor]);

        let second = page(
            session
                .read_page(first_page.next_delta_cursor, 1, 1024 * 1024)
                .unwrap(),
        );
        let (second_page, second_payload) =
            decode_remote_delta_journal_page_classified(&second, inputs(at(0, 0), at(3, 0)))
                .unwrap();
        assert!(!second_page.has_more);
        assert_eq!(second_page.from_sequence, 2);
        assert_eq!(second_page.through_sequence, 2);
        assert_eq!(second_payload.project_descriptors, vec![second_descriptor]);
    }

    #[test]
    fn descriptor_union_bound_can_be_satisfied_by_contiguous_pages() {
        let first_descriptor = descriptor(1);
        let second_descriptor = descriptor(2);
        let changes = [
            encode_remote_delta_journal_record(
                bucket_record(at(1, 0), first_descriptor.observed_project_key.clone()),
                vec![first_descriptor.clone()],
            )
            .unwrap(),
            encode_remote_delta_journal_record(
                bucket_record(at(1, 15), second_descriptor.observed_project_key.clone()),
                vec![second_descriptor.clone()],
            )
            .unwrap(),
        ];
        let directory = tempdir().unwrap();
        let store = RemoteExportStateStore::new(
            directory.path().join("state"),
            source(),
            RedactionProfile::Redacted,
        );
        let mut session = store.try_begin(at(2, 0)).unwrap();
        let initial = session.status().unwrap().cursor;
        session.append_changes(at(2, 0), &changes).unwrap();

        let combined = page(session.read_page(initial, 16, 1024 * 1024).unwrap());
        let error = decode_remote_delta_journal_page_with_descriptor_limit(
            &combined,
            inputs(at(0, 0), at(3, 0)),
            1,
        )
        .unwrap_err();
        assert!(error.is_splittable());

        let first = page(session.read_page(initial, 1, 1024 * 1024).unwrap());
        let (first_page, _) = decode_remote_delta_journal_page_with_descriptor_limit(
            &first,
            inputs(at(0, 0), at(3, 0)),
            1,
        )
        .unwrap();
        let second = page(
            session
                .read_page(first_page.next_delta_cursor, 1, 1024 * 1024)
                .unwrap(),
        );
        let (second_page, _) = decode_remote_delta_journal_page_with_descriptor_limit(
            &second,
            inputs(at(0, 0), at(3, 0)),
            1,
        )
        .unwrap();
        assert_eq!(first_page.through_sequence + 1, second_page.from_sequence);
        assert_eq!(second_page.through_sequence, 2);
    }

    #[test]
    fn invalid_single_record_is_fatal_instead_of_splittable() {
        let descriptor = descriptor(1);
        let mut record = bucket_record(at(1, 0), descriptor.observed_project_key.clone());
        let RemoteDeltaJournalRecord::UsageBucket { mutation, .. } = &mut record else {
            unreachable!();
        };
        let RemoteUsageBucketMutation::Upsert(bucket) = mutation else {
            unreachable!();
        };
        bucket.model_groups.push(RemoteModelUsageGroup {
            model: Some("x".repeat(257)),
            service_tier: Some("standard".to_owned()),
            token_usage: tokens(),
            estimated_cost_units: RemoteU128::new(75),
            api_long_context_extra_cost_units: None,
            api_equivalent_cost: api_cost(),
            call_count: 1,
            used_model_fallback: false,
            used_token_breakdown_fallback: false,
            used_long_context_pricing: false,
            used_long_context_detection_fallback: false,
        });
        let change = encode_remote_delta_journal_record(record, vec![descriptor]).unwrap();
        let directory = tempdir().unwrap();
        let store = RemoteExportStateStore::new(
            directory.path().join("state"),
            source(),
            RedactionProfile::Redacted,
        );
        let mut session = store.try_begin(at(2, 0)).unwrap();
        let cursor = session.status().unwrap().cursor;
        session.append_changes(at(2, 0), &[change]).unwrap();
        let durable = page(session.read_page(cursor, 1, 1024 * 1024).unwrap());
        let error =
            decode_remote_delta_journal_page_classified(&durable, inputs(at(0, 0), at(3, 0)))
                .unwrap_err();
        assert!(!error.is_splittable());
        assert!(error.to_string().contains("model"));
    }

    #[test]
    fn desired_record_validation_rejects_all_model_and_service_tier_text_hazards() {
        let cases = [
            (Some("x".repeat(257)), Some("standard".to_owned())),
            (Some("bad\nmodel".to_owned()), Some("standard".to_owned())),
            (Some("gpt-5.6-sol".to_owned()), Some("x".repeat(65))),
            (Some("gpt-5.6-sol".to_owned()), Some("bad\ttier".to_owned())),
        ];
        for (model, service_tier) in cases {
            let descriptor = descriptor(1);
            let starts_at = at(1, 0);
            let mut record = bucket_record(starts_at, descriptor.observed_project_key.clone());
            let RemoteDeltaJournalRecord::UsageBucket { mutation, .. } = &mut record else {
                unreachable!();
            };
            let RemoteUsageBucketMutation::Upsert(bucket) = mutation else {
                unreachable!();
            };
            bucket.model_groups.push(RemoteModelUsageGroup {
                model,
                service_tier,
                token_usage: tokens(),
                estimated_cost_units: RemoteU128::new(75),
                api_long_context_extra_cost_units: None,
                api_equivalent_cost: api_cost(),
                call_count: 1,
                used_model_fallback: false,
                used_token_breakdown_fallback: false,
                used_long_context_pricing: false,
                used_long_context_detection_fallback: false,
            });
            let upsert = encode_remote_delta_journal_record(record, vec![descriptor]).unwrap();
            let tombstone =
                encode_remote_delta_journal_record(tombstone_bucket(starts_at), Vec::new())
                    .unwrap();
            let desired = RemoteExportDesiredRecord::new(
                format!("bucket-{}", starts_at.timestamp()),
                starts_at + Duration::days(35),
                upsert,
                tombstone,
            )
            .unwrap();
            let error = validate_remote_delta_desired_records(
                std::slice::from_ref(&desired),
                &inputs(at(0, 0), at(3, 0)),
            )
            .unwrap_err();
            assert!(
                error.to_string().contains("model") || error.to_string().contains("service tier")
            );
        }
    }

    #[test]
    fn materialized_aba_reuses_canonical_content_at_new_sequence_revisions() {
        let descriptor = descriptor(1);
        let starts_at = at(1, 0);
        let upsert = encode_remote_delta_journal_record(
            bucket_record(starts_at, descriptor.observed_project_key.clone()),
            vec![descriptor.clone()],
        )
        .unwrap();
        let tombstone =
            encode_remote_delta_journal_record(tombstone_bucket(starts_at), Vec::new()).unwrap();
        let desired = RemoteExportDesiredRecord::new(
            "bucket-1",
            at(23, 59) + Duration::days(35),
            upsert,
            tombstone,
        )
        .unwrap();
        let directory = tempdir().unwrap();
        let store = RemoteExportStateStore::new(
            directory.path().join("state"),
            source(),
            RedactionProfile::Redacted,
        );
        let mut session = store.try_begin(at(2, 0)).unwrap();
        let initial = session.status().unwrap().cursor;
        session
            .reconcile_materialized_records(
                at(2, 0),
                std::slice::from_ref(&desired),
                RemoteExportReconcileMode::Authoritative,
            )
            .unwrap();
        session
            .reconcile_materialized_records(at(2, 1), &[], RemoteExportReconcileMode::Authoritative)
            .unwrap();
        session
            .reconcile_materialized_records(
                at(2, 2),
                std::slice::from_ref(&desired),
                RemoteExportReconcileMode::Authoritative,
            )
            .unwrap();

        let durable = page(session.read_page(initial, 16, 1024 * 1024).unwrap());
        assert_eq!(
            durable
                .entries
                .iter()
                .map(|entry| entry.change().change_id())
                .collect::<Vec<_>>(),
            vec![
                desired.upsert().change_id(),
                desired.tombstone().change_id(),
                desired.upsert().change_id(),
            ]
        );
        let (_, payload) =
            decode_remote_delta_journal_page(&durable, inputs(at(0, 0), at(3, 0))).unwrap();
        assert_eq!(payload.project_descriptors, vec![descriptor]);
        assert_eq!(
            payload
                .bucket_changes
                .iter()
                .map(|change| (change.sequence.get(), change.revision.get()))
                .collect::<Vec<_>>(),
            vec![(1, 1), (2, 2), (3, 3)]
        );
    }

    #[test]
    fn corrupt_hash_noncanonical_encoding_and_key_mismatch_fail_closed() {
        let descriptor = descriptor(1);
        let valid = encode_remote_delta_journal_record(
            bucket_record(at(1, 0), descriptor.observed_project_key.clone()),
            vec![descriptor.clone()],
        )
        .unwrap();

        let bad_hash =
            RemoteExportChange::new("wrong-content-id", valid.payload().to_vec()).unwrap();
        assert_decode_error_contains(bad_hash, "content hash");

        let mut noncanonical_payload = valid.payload().to_vec();
        noncanonical_payload.push(b' ');
        let noncanonical = RemoteExportChange::new(
            content_derived_change_id(&noncanonical_payload),
            noncanonical_payload,
        )
        .unwrap();
        assert_decode_error_contains(noncanonical, "not in canonical encoding");

        let wrong_kind_payload = String::from_utf8(valid.payload().to_vec())
            .unwrap()
            .replace("\"usageBucket\"", "\"sessionDigest\"")
            .into_bytes();
        let wrong_kind = RemoteExportChange::new(
            content_derived_change_id(&wrong_kind_payload),
            wrong_kind_payload,
        )
        .unwrap();
        assert_decode_error_contains(wrong_kind, "invalid remote delta journal record");

        let mismatched = VersionedRemoteDeltaJournalRecord::V1(RemoteDeltaJournalRecordV1 {
            record: RemoteDeltaJournalRecord::UsageBucket {
                starts_at: at(1, 15),
                mutation: match bucket_record(at(1, 0), descriptor.observed_project_key.clone()) {
                    RemoteDeltaJournalRecord::UsageBucket { mutation, .. } => mutation,
                    RemoteDeltaJournalRecord::SessionDigest { .. } => unreachable!(),
                },
            },
            project_descriptors: vec![descriptor],
        });
        let payload = encode_canonical_bounded(&mismatched).unwrap();
        let change = RemoteExportChange::new(content_derived_change_id(&payload), payload).unwrap();
        assert_decode_error_contains(change, "key does not match its payload");
    }

    #[test]
    fn descriptors_and_content_id_collisions_are_rejected() {
        let first = descriptor(1);
        let mut conflicting = first.clone();
        conflicting.display_label = "different".parse().unwrap();
        let record = bucket_record(at(1, 0), first.observed_project_key.clone());
        let error =
            encode_remote_delta_journal_record(record.clone(), vec![first.clone(), conflicting])
                .unwrap_err();
        assert!(error.to_string().contains("conflicting descriptors"));

        let error = encode_remote_delta_journal_record(record, Vec::new()).unwrap_err();
        assert!(error.to_string().contains("exactly match"));

        let mut unsafe_descriptor = first.clone();
        unsafe_descriptor.git_evidence =
            crate::remote_protocol::RemoteGitRepositoryEvidence::Repository {
                fingerprint: None,
                repository_relative_workspace_root: "../secret".to_owned(),
            };
        let error = encode_remote_delta_journal_record(
            bucket_record(at(1, 0), first.observed_project_key.clone()),
            vec![unsafe_descriptor],
        )
        .unwrap_err();
        assert!(error.to_string().contains("repository-relative"));

        let mut ids = HashMap::new();
        remember_content_id(&mut ids, "same-id", b"first").unwrap();
        let collision = remember_content_id(&mut ids, "same-id", b"second").unwrap_err();
        assert!(collision.to_string().contains("content-hash collision"));
    }

    #[test]
    fn protocol_invalid_record_and_invalid_page_semantics_fail_closed() {
        let descriptor = descriptor(1);
        let mut record = bucket_record(at(3, 0), descriptor.observed_project_key.clone());
        let RemoteDeltaJournalRecord::UsageBucket { mutation, .. } = &mut record else {
            unreachable!();
        };
        let RemoteUsageBucketMutation::Upsert(bucket) = mutation else {
            unreachable!();
        };
        bucket.sampled_at = at(5, 0);
        let invalid = encode_remote_delta_journal_record(record, vec![descriptor]).unwrap();
        assert_decode_error_contains_with_range(invalid, "sample time", at(0, 0), at(2, 0));

        let directory = tempdir().unwrap();
        let store = RemoteExportStateStore::new(
            directory.path().join("state"),
            source(),
            RedactionProfile::Redacted,
        );
        let session = store.try_begin(at(2, 0)).unwrap();
        let cursor = session.status().unwrap().cursor;
        let mut durable = page(session.read_page(cursor, 16, 1024 * 1024).unwrap());
        durable.has_more = true;
        let error =
            decode_remote_delta_journal_page(&durable, inputs(at(0, 0), at(3, 0))).unwrap_err();
        assert!(error.to_string().contains("no-change page"));
    }

    fn assert_decode_error_contains(change: RemoteExportChange, expected: &str) {
        assert_decode_error_contains_with_range(change, expected, at(0, 0), at(3, 0));
    }

    fn assert_decode_error_contains_with_range(
        change: RemoteExportChange,
        expected: &str,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) {
        let directory = tempdir().unwrap();
        let store = RemoteExportStateStore::new(
            directory.path().join("state"),
            source(),
            RedactionProfile::Redacted,
        );
        let mut session = store.try_begin(at(2, 0)).unwrap();
        let cursor = session.status().unwrap().cursor;
        session.append_changes(at(2, 0), &[change]).unwrap();
        let durable = page(session.read_page(cursor, 16, 1024 * 1024).unwrap());
        let error = decode_remote_delta_journal_page(&durable, inputs(from, to)).unwrap_err();
        assert!(
            error.to_string().contains(expected),
            "expected {expected:?} in {error}"
        );
    }
}
