//! Durable, source-bound state for the remote aggregate delta exporter.
//!
//! This module deliberately does not scan rollout files or advertise a remote
//! capability. It owns only the persistence and cursor semantics needed by a
//! future exporter. One source generation has one non-blocking exporter lock;
//! redacted and preview journals remain physically and cryptographically
//! independent beneath that lock.

use std::collections::{BTreeMap, HashMap};
use std::ffi::OsStr;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use crate::atomic_file::replace_file;
use crate::remote_protocol::{
    DeltaCursor, RemoteDeltaWarning, RemoteGitRepositoryEvidence, RemoteLiveSnapshot,
    RemoteLiveState, RemoteProjectDescriptor, SourceGeneration,
    validate_remote_partial_reasons_for_storage,
};
use crate::source_history::RedactionProfile;

const EXPORT_LAYOUT_DIRECTORY: &str = "remote-export-v1";
const DELTA_STATE_FILE: &str = "delta-state.json";
const DELTA_ANCHOR_FILE: &str = "delta-state.anchor";
const EXPORT_LOCK_FILE: &str = "remote-export.lock";
const STATE_FORMAT_VERSION: u32 = 3;
const ANCHOR_FORMAT_VERSION: u32 = 1;
const JOURNAL_RETENTION_DAYS: i64 = 35;
const MAX_JOURNAL_STATE_BYTES: u64 = 128 * 1024 * 1024;
const MAX_JOURNAL_ENTRIES: usize = 262_144;
const MAX_CHANGE_ID_BYTES: usize = 192;
const MAX_CHANGE_PAYLOAD_BYTES: usize = 1024 * 1024;
const MAX_APPEND_BATCH_ENTRIES: usize = 4096;
const MAX_MATERIALIZED_RECORDS: usize = 65_536;
const MAX_MATERIALIZED_KEY_BYTES: usize = 512;
const MAX_PAGE_ENTRIES: usize = 4096;
const MAX_PAGE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_ANCHOR_BYTES: u64 = 4096;
const MAX_LIVE_PROJECT_DESCRIPTORS: usize = 16_384;
const RETENTION_CLOCK_MAX_UNCONFIRMED_FORWARD_HOURS: i64 = 24;
const RETENTION_CLOCK_CONFIRMATION_HOURS: i64 = 48;
const RETENTION_CLOCK_MIN_CONFIRMATIONS: u32 = 3;
const RECONCILE_MAINTENANCE_INTERVAL_HOURS: i64 = 12;
const TEMP_FILE_ATTEMPTS: usize = 128;

static TEMP_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn validate_change_id(change_id: &str) -> io::Result<()> {
    if change_id.is_empty()
        || change_id.len() > MAX_CHANGE_ID_BYTES
        || !change_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
    {
        return Err(invalid_data(
            "remote export change ID must be bounded path-safe ASCII",
        ));
    }
    Ok(())
}

fn validate_materialized_key(logical_key: &str) -> io::Result<()> {
    if logical_key.is_empty()
        || logical_key.len() > MAX_MATERIALIZED_KEY_BYTES
        || !logical_key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
    {
        return Err(invalid_data(
            "remote materialized logical key must be bounded path-safe ASCII",
        ));
    }
    Ok(())
}

fn validate_materialized_id_owner<'a>(
    owners: &mut HashMap<&'a str, (&'a str, &'static str)>,
    change_id: &'a str,
    logical_key: &'a str,
    role: &'static str,
) -> io::Result<()> {
    if let Some((existing_key, existing_role)) = owners.insert(change_id, (logical_key, role)) {
        return Err(invalid_data(format!(
            "remote materialized change ID is shared by {existing_key}/{existing_role} and {logical_key}/{role}"
        )));
    }
    Ok(())
}

/// One opaque aggregate payload waiting to be adapted to the wire protocol.
/// `change_id` is a deterministic content identity: direct append retries are
/// idempotent, while a real materialized-set ABA transition may append the
/// same ID and bytes at a new journal sequence. Reusing an ID with different
/// bytes is always durable corruption.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RemoteExportChange {
    change_id: String,
    payload: Vec<u8>,
}

impl RemoteExportChange {
    pub fn new(change_id: impl Into<String>, payload: Vec<u8>) -> io::Result<Self> {
        let change = Self {
            change_id: change_id.into(),
            payload,
        };
        change.validate()?;
        Ok(change)
    }

    pub fn change_id(&self) -> &str {
        &self.change_id
    }

    pub fn payload(&self) -> &[u8] {
        &self.payload
    }

    fn validate(&self) -> io::Result<()> {
        validate_change_id(&self.change_id)?;
        if self.payload.len() > MAX_CHANGE_PAYLOAD_BYTES {
            return Err(invalid_data("remote export change payload is too large"));
        }
        Ok(())
    }
}

/// One desired member of the source/profile materialized aggregate set.
/// Callers must pass records in strictly increasing `logical_key` order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteExportDesiredRecord {
    logical_key: String,
    expires_at: DateTime<Utc>,
    upsert: RemoteExportChange,
    tombstone: RemoteExportChange,
}

impl RemoteExportDesiredRecord {
    pub fn new(
        logical_key: impl Into<String>,
        expires_at: DateTime<Utc>,
        upsert: RemoteExportChange,
        tombstone: RemoteExportChange,
    ) -> io::Result<Self> {
        let record = Self {
            logical_key: logical_key.into(),
            expires_at,
            upsert,
            tombstone,
        };
        record.validate()?;
        Ok(record)
    }

    pub fn logical_key(&self) -> &str {
        &self.logical_key
    }

    pub fn upsert(&self) -> &RemoteExportChange {
        &self.upsert
    }

    /// Stable end of this aggregate's publication lifetime. Reconciliation
    /// persists this value with the current set; omission from a partial scan
    /// never changes it or acts as deletion evidence.
    pub fn expires_at(&self) -> DateTime<Utc> {
        self.expires_at
    }

    pub fn tombstone(&self) -> &RemoteExportChange {
        &self.tombstone
    }

    fn validate(&self) -> io::Result<()> {
        validate_materialized_key(&self.logical_key)?;
        self.upsert.validate()?;
        self.tombstone.validate()?;
        if self.upsert.change_id == self.tombstone.change_id {
            return Err(invalid_data(
                "remote materialized upsert and tombstone must have distinct change IDs",
            ));
        }
        Ok(())
    }
}

/// Whether absence from `desired` is merely unknown or authoritative proof
/// that the old materialized record disappeared.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemoteExportReconcileMode {
    /// Apply supplied upserts but retain old keys omitted by a partial scan.
    UpsertOnly,
    /// Treat `desired` as the complete domain and tombstone omitted old keys.
    Authoritative,
}

/// A committed change. Sequences are global within exactly one journal
/// generation and remain contiguous above `retention_floor`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RemoteExportJournalEntry {
    sequence: u64,
    committed_at: DateTime<Utc>,
    change: RemoteExportChange,
}

impl RemoteExportJournalEntry {
    pub fn sequence(&self) -> u64 {
        self.sequence
    }

    pub fn committed_at(&self) -> DateTime<Utc> {
        self.committed_at
    }

    pub fn change(&self) -> &RemoteExportChange {
        &self.change
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemoteDeltaCursorExpiryReason {
    GenerationMismatch,
    Retention,
}

/// Typed cursor expiry. This is a recoverable bootstrap signal and must never
/// be collapsed into an empty page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RemoteDeltaCursorExpired {
    pub reason: RemoteDeltaCursorExpiryReason,
    pub requested: DeltaCursor,
    pub current_generation: NonZeroU64,
    /// Highest sequence that has been discarded. A cursor equal to this floor
    /// is still valid; a lower cursor is expired.
    pub retention_floor: u64,
    pub through_sequence: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteExportDeltaPage {
    pub generation: NonZeroU64,
    pub from_sequence: u64,
    pub through_sequence: u64,
    pub next_cursor: DeltaCursor,
    pub has_more: bool,
    pub entries: Vec<RemoteExportJournalEntry>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RemoteDeltaPageRead {
    Page(RemoteExportDeltaPage),
    CursorExpired(RemoteDeltaCursorExpired),
}

/// Result of reconciling a desired materialized set against a private copy of
/// the durable state and measuring the resulting delta without publishing it.
/// This lets callers reject an unfinishable fixed-watermark batch before the
/// authoritative index is changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RemoteExportReconcileDeltaPreflight {
    Ready {
        cursor: DeltaCursor,
        entries: usize,
        serialized_bytes: usize,
    },
    CursorExpired(RemoteDeltaCursorExpired),
    LimitExceeded,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RemoteExportStatus {
    pub cursor: DeltaCursor,
    pub retention_floor: u64,
    pub retained_entries: usize,
    pub materialized_records: usize,
    pub encoded_bytes: u64,
    pub live_revision: Option<NonZeroU64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteExportLivePage {
    pub state: RemoteLiveState,
    pub project_descriptors: Vec<RemoteProjectDescriptor>,
    pub partial_reasons: Vec<String>,
    pub warnings: Vec<RemoteDeltaWarning>,
}

/// Current materialized upsert plus the journal sequence that most recently
/// established it. Fact snapshots use this read-only view while retaining the
/// journal's revision semantics; callers never receive tombstone templates or
/// mutable access to the durable set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteExportMaterializedUpsert {
    logical_key: String,
    expires_at: DateTime<Utc>,
    revision: u64,
    change: RemoteExportChange,
}

impl RemoteExportMaterializedUpsert {
    pub fn logical_key(&self) -> &str {
        &self.logical_key
    }

    pub fn expires_at(&self) -> DateTime<Utc> {
        self.expires_at
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn change(&self) -> &RemoteExportChange {
        &self.change
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RemoteExportAppendReport {
    pub cursor: DeltaCursor,
    pub appended: usize,
    pub duplicates: usize,
    pub pruned_by_age: usize,
    pub pruned_by_capacity: usize,
    pub retention_floor: u64,
    /// Current upserts re-appended after retention/capacity removed their last
    /// materialized transition. These are not caller-supplied direct changes.
    pub materialized_refreshes: usize,
    /// Current materialized records removed after their persisted TTL passed.
    pub expired_records: usize,
    /// True when a suspicious forward clock step deferred age-based deletion.
    pub retention_deferred: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RemoteExportReconcileReport {
    pub cursor: DeltaCursor,
    pub upserts_appended: usize,
    pub tombstones_appended: usize,
    /// Subset of tombstones caused solely by persisted TTL expiry.
    pub expired_records: usize,
    /// Upserts appended only to make a floor-based bootstrap self-contained.
    pub bootstrap_upserts_appended: usize,
    pub current_records: usize,
    pub pruned_by_age: usize,
    pub pruned_by_capacity: usize,
    pub retention_floor: u64,
    pub retention_deferred: bool,
}

#[derive(Clone, Copy, Debug)]
struct RemoteExportLimits {
    retention_days: i64,
    maximum_state_bytes: u64,
    maximum_entries: usize,
    maximum_page_entries: usize,
    maximum_page_bytes: u64,
}

impl Default for RemoteExportLimits {
    fn default() -> Self {
        Self {
            retention_days: JOURNAL_RETENTION_DAYS,
            maximum_state_bytes: MAX_JOURNAL_STATE_BYTES,
            maximum_entries: MAX_JOURNAL_ENTRIES,
            maximum_page_entries: MAX_PAGE_ENTRIES,
            maximum_page_bytes: MAX_PAGE_BYTES,
        }
    }
}

/// A filesystem namespace for one complete source generation and redaction
/// profile. Construction is side-effect free; `try_begin` performs the first
/// private-directory creation and takes the source-wide exporter lock.
#[derive(Clone, Debug)]
pub struct RemoteExportStateStore {
    root: PathBuf,
    source: SourceGeneration,
    redaction_profile: RedactionProfile,
    limits: RemoteExportLimits,
}

impl RemoteExportStateStore {
    pub fn new(
        root: impl Into<PathBuf>,
        source: SourceGeneration,
        redaction_profile: RedactionProfile,
    ) -> Self {
        Self {
            root: root.into(),
            source,
            redaction_profile,
            limits: RemoteExportLimits::default(),
        }
    }

    pub fn source(&self) -> &SourceGeneration {
        &self.source
    }

    pub fn redaction_profile(&self) -> RedactionProfile {
        self.redaction_profile
    }

    pub fn namespace_directory(&self) -> PathBuf {
        self.source_directory()
            .join(self.redaction_profile.directory_name())
    }

    /// Attempts to become the only exporter for this source generation.
    /// Contention is returned immediately as `WouldBlock`.
    pub fn try_begin(&self, observed_at: DateTime<Utc>) -> io::Result<RemoteExportSession<'_>> {
        self.prepare_source_directory()?;
        let lock = try_open_source_lock(self, &self.source_directory())?;
        self.prepare_profile_directory()?;
        let state = self.load_or_create_state(observed_at)?;
        let encoded_bytes = encoded_state_size(&state)?;
        Ok(RemoteExportSession {
            store: self,
            lock,
            state: Some(state),
            encoded_bytes,
        })
    }

    fn export_root(&self) -> PathBuf {
        self.root.join(EXPORT_LAYOUT_DIRECTORY)
    }

    fn node_directory(&self) -> PathBuf {
        self.export_root().join(self.source.node_id.as_str())
    }

    fn source_directory(&self) -> PathBuf {
        self.node_directory()
            .join(format!("generation-{}", self.source.generation))
    }

    fn prepare_source_directory(&self) -> io::Result<()> {
        prepare_private_root(&self.root)?;
        let export_root = self.export_root();
        create_private_child_directory(&self.root, &export_root)?;
        let node_directory = self.node_directory();
        create_private_child_directory(&export_root, &node_directory)?;
        create_private_child_directory(&node_directory, &self.source_directory())
    }

    fn prepare_profile_directory(&self) -> io::Result<()> {
        let source_directory = self.source_directory();
        validate_private_directory(&source_directory, "remote export source directory")?;
        create_private_child_directory(&source_directory, &self.namespace_directory())
    }

    fn validate_namespace(&self) -> io::Result<()> {
        validate_private_directory(&self.root, "remote export state root")?;
        validate_private_directory(&self.export_root(), "remote export layout directory")?;
        validate_private_directory(&self.node_directory(), "remote export node directory")?;
        validate_private_directory(&self.source_directory(), "remote export source directory")?;
        validate_private_directory(
            &self.namespace_directory(),
            "remote export profile directory",
        )
    }

    fn state_path(&self) -> PathBuf {
        self.namespace_directory().join(DELTA_STATE_FILE)
    }

    fn anchor_path(&self) -> PathBuf {
        self.namespace_directory().join(DELTA_ANCHOR_FILE)
    }

    fn load_or_create_state(&self, observed_at: DateTime<Utc>) -> io::Result<StoredExportState> {
        self.validate_namespace()?;
        match self.read_state() {
            Ok(state) => {
                self.ensure_anchor(&state)?;
                Ok(state)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                match self.read_anchor() {
                    Ok(_) => {
                        return Err(invalid_data(
                            "remote export state is missing from an initialized namespace",
                        ));
                    }
                    Err(anchor_error) if anchor_error.kind() == io::ErrorKind::NotFound => {}
                    Err(anchor_error) => return Err(anchor_error),
                }
                let state = StoredExportState::new(
                    self.source.clone(),
                    self.redaction_profile,
                    observed_at,
                )?;
                self.write_state(&state)?;
                self.ensure_anchor(&state)?;
                Ok(state)
            }
            Err(error) => Err(error),
        }
    }

    fn read_state(&self) -> io::Result<StoredExportState> {
        self.validate_namespace()?;
        let state: StoredExportState = read_private_json_file(
            &self.state_path(),
            self.limits.maximum_state_bytes,
            "remote export state",
        )?;
        state.validate(self)?;
        Ok(state)
    }

    fn write_state(&self, state: &StoredExportState) -> io::Result<u64> {
        self.validate_namespace()?;
        state.validate(self)?;
        write_private_json_atomically(
            &self.state_path(),
            state,
            self.limits.maximum_state_bytes,
            "remote export state",
        )
    }

    fn read_anchor(&self) -> io::Result<StoredExportAnchor> {
        self.validate_namespace()?;
        let anchor: StoredExportAnchor = read_private_json_file(
            &self.anchor_path(),
            MAX_ANCHOR_BYTES,
            "remote export anchor",
        )?;
        anchor.validate(self)?;
        Ok(anchor)
    }

    fn ensure_anchor(&self, state: &StoredExportState) -> io::Result<()> {
        let expected = StoredExportAnchor::from_state(state);
        match self.read_anchor() {
            Ok(anchor) if anchor == expected => Ok(()),
            Ok(_) => Err(invalid_data(
                "remote export anchor does not match its durable state",
            )),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                if create_private_json_once(
                    &self.anchor_path(),
                    &expected,
                    MAX_ANCHOR_BYTES,
                    "remote export anchor",
                )? {
                    Ok(())
                } else {
                    let anchor = self.read_anchor()?;
                    if anchor == expected {
                        Ok(())
                    } else {
                        Err(invalid_data(
                            "remote export anchor raced with incompatible state",
                        ))
                    }
                }
            }
            Err(error) => Err(error),
        }
    }

    #[cfg(test)]
    fn with_limits(mut self, limits: RemoteExportLimits) -> Self {
        self.limits = limits;
        self
    }
}

/// Holds the source-wide OS lock for one exporter transaction/session.
pub struct RemoteExportSession<'a> {
    store: &'a RemoteExportStateStore,
    lock: File,
    state: Option<StoredExportState>,
    encoded_bytes: u64,
}

impl fmt::Debug for RemoteExportSession<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RemoteExportSession")
            .field("source", &self.store.source)
            .field("redaction_profile", &self.store.redaction_profile)
            .field("state_loaded", &self.state.is_some())
            .finish_non_exhaustive()
    }
}

impl RemoteExportSession<'_> {
    pub fn status(&self) -> io::Result<RemoteExportStatus> {
        self.validate_fence()?;
        let state = self.state()?;
        Ok(RemoteExportStatus {
            cursor: state.cursor(),
            retention_floor: state.retention_floor,
            retained_entries: state.entries.len(),
            materialized_records: state.materialized_records.len(),
            encoded_bytes: self.encoded_bytes,
            live_revision: state.live.as_ref().map(|live| live.revision),
        })
    }

    /// Returns the durable live replacement without touching rollout files.
    /// A forced copy is used for a cursorless/bootstrap center; continuation
    /// pages normally carry only the stable revision.
    pub fn current_live_page(
        &self,
        known_live_revision: Option<NonZeroU64>,
    ) -> io::Result<Option<RemoteExportLivePage>> {
        self.validate_fence()?;
        Ok(self
            .state()?
            .live
            .as_ref()
            .map(|live| live.page(known_live_revision != Some(live.revision))))
    }

    /// Reconciles semantic live rows independently from `capturedAt`.
    /// Successful scans with no row/descriptor changes therefore preserve the
    /// revision and emit revision-only, while an actual replacement is
    /// published atomically with the exporter journal state.
    pub fn reconcile_live_snapshot(
        &mut self,
        observed_at: DateTime<Utc>,
        snapshot: RemoteLiveSnapshot,
        project_descriptors: Vec<RemoteProjectDescriptor>,
        partial_reasons: Vec<String>,
        warnings: Vec<RemoteDeltaWarning>,
        known_live_revision: Option<NonZeroU64>,
    ) -> io::Result<RemoteExportLivePage> {
        self.validate_fence()?;
        let project_descriptors =
            preserve_unavailable_git_evidence(self.state()?.live.as_ref(), project_descriptors);
        validate_live_parts(
            self.store.redaction_profile,
            observed_at,
            &snapshot,
            &project_descriptors,
        )?;
        validate_live_quality(&partial_reasons, &warnings)?;

        let changed = self.state()?.live.as_ref().is_none_or(|current| {
            current.snapshot.tasks != snapshot.tasks
                || current.snapshot.turns != snapshot.turns
                || current.project_descriptors != project_descriptors
        });
        if !changed {
            let quality_changed = self.state()?.live.as_ref().is_some_and(|current| {
                current.partial_reasons != partial_reasons || current.warnings != warnings
            });
            if quality_changed {
                let mut state = self.take_state()?;
                let live = state.live.as_mut().expect("unchanged live state exists");
                live.partial_reasons = partial_reasons;
                live.warnings = warnings;
                self.validate_fence()?;
                match self.store.write_state(&state) {
                    Ok(encoded_bytes) => {
                        self.validate_fence()?;
                        self.encoded_bytes = encoded_bytes;
                        self.state = Some(state);
                    }
                    Err(error) => {
                        self.restore_after_failed_mutation()?;
                        return Err(error);
                    }
                }
            }
            return Ok(self
                .state()?
                .live
                .as_ref()
                .expect("unchanged live state exists")
                .page(
                    known_live_revision
                        != Some(
                            self.state()?
                                .live
                                .as_ref()
                                .expect("live state exists")
                                .revision,
                        ),
                ));
        }

        let revision = match self.state()?.live.as_ref() {
            Some(current) => NonZeroU64::new(
                current
                    .revision
                    .get()
                    .checked_add(1)
                    .ok_or_else(|| invalid_data("remote live revision overflowed"))?,
            )
            .expect("checked non-zero live revision"),
            None => NonZeroU64::new(1).expect("one is non-zero"),
        };
        let live = StoredLiveState {
            revision,
            snapshot,
            project_descriptors,
            partial_reasons,
            warnings,
        };
        let mut state = self.take_state()?;
        state.live = Some(live.clone());
        self.validate_fence()?;
        match self.store.write_state(&state) {
            Ok(encoded_bytes) => {
                self.validate_fence()?;
                self.encoded_bytes = encoded_bytes;
                self.state = Some(state);
                Ok(live.page(true))
            }
            Err(error) => {
                self.restore_after_failed_mutation()?;
                Err(error)
            }
        }
    }

    pub fn materialized_upserts(&self) -> io::Result<Vec<RemoteExportMaterializedUpsert>> {
        self.validate_fence()?;
        let state = self.state()?;
        let upserts = capture_materialized_upserts(state)?;
        let mut latest_revision = HashMap::<&str, u64>::new();
        for entry in &state.entries {
            latest_revision.insert(entry.change.change_id(), entry.sequence());
        }
        state
            .materialized_records
            .iter()
            .map(|record| {
                let change = upserts.get(&record.logical_key).cloned().ok_or_else(|| {
                    invalid_data("remote materialized upsert disappeared from its journal")
                })?;
                let revision = latest_revision
                    .get(record.current_change_id.as_str())
                    .copied()
                    .ok_or_else(|| {
                        invalid_data("remote materialized upsert has no journal revision")
                    })?;
                Ok(RemoteExportMaterializedUpsert {
                    logical_key: record.logical_key.clone(),
                    expires_at: record.expires_at,
                    revision,
                    change,
                })
            })
            .collect()
    }

    pub fn append_changes(
        &mut self,
        observed_at: DateTime<Utc>,
        changes: &[RemoteExportChange],
    ) -> io::Result<RemoteExportAppendReport> {
        self.validate_fence()?;
        if changes.len() > MAX_APPEND_BATCH_ENTRIES {
            return Err(invalid_data("remote export append batch is too large"));
        }
        for change in changes {
            change.validate()?;
        }

        let mut state = self.take_state()?;
        let result = append_and_compact(&mut state, observed_at, changes, self.store);
        let report = match result {
            Ok(report) => report,
            Err(error) => {
                self.restore_after_failed_mutation()?;
                return Err(error);
            }
        };
        self.validate_fence()?;
        match self.store.write_state(&state) {
            Ok(encoded_bytes) => {
                self.validate_fence()?;
                self.encoded_bytes = encoded_bytes;
                self.state = Some(state);
                Ok(report)
            }
            Err(error) => {
                self.restore_after_failed_mutation()?;
                Err(error)
            }
        }
    }

    /// Reconciles one canonical desired materialized set under the exporter
    /// lock and publishes journal transitions plus the new current set in one
    /// atomic state-file replacement.
    pub fn reconcile_materialized_records(
        &mut self,
        observed_at: DateTime<Utc>,
        desired: &[RemoteExportDesiredRecord],
        mode: RemoteExportReconcileMode,
    ) -> io::Result<RemoteExportReconcileReport> {
        self.validate_fence()?;
        validate_desired_records(desired)?;

        let current = self.state()?;
        if materialized_set_matches(current, desired)?
            && !reconcile_maintenance_due(current, observed_at, self.store)
        {
            return Ok(RemoteExportReconcileReport {
                cursor: current.cursor(),
                upserts_appended: 0,
                tombstones_appended: 0,
                expired_records: 0,
                bootstrap_upserts_appended: 0,
                current_records: current.materialized_records.len(),
                pruned_by_age: 0,
                pruned_by_capacity: 0,
                retention_floor: current.retention_floor,
                retention_deferred: current.retention_clock.pending_started_at.is_some(),
            });
        }

        let mut state = self.take_state()?;
        let result = reconcile_and_compact(&mut state, observed_at, desired, mode, self.store);
        let report = match result {
            Ok(report) => report,
            Err(error) => {
                self.restore_after_failed_mutation()?;
                return Err(error);
            }
        };
        self.validate_fence()?;
        match self.store.write_state(&state) {
            Ok(encoded_bytes) => {
                self.validate_fence()?;
                self.encoded_bytes = encoded_bytes;
                self.state = Some(state);
                Ok(report)
            }
            Err(error) => {
                self.restore_after_failed_mutation()?;
                Err(error)
            }
        }
    }

    /// Simulates a reconcile and streams the prospective cursor-to-watermark
    /// delta through `serialized_record_bytes`. The real state and state file
    /// remain untouched, including when retention makes the cursor unusable or
    /// the caller's complete-batch envelope is exceeded.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn preflight_reconciled_delta<F>(
        &self,
        observed_at: DateTime<Utc>,
        desired: &[RemoteExportDesiredRecord],
        mode: RemoteExportReconcileMode,
        cursor: DeltaCursor,
        maximum_entries: usize,
        initial_serialized_bytes: usize,
        maximum_record_serialized_bytes: usize,
        maximum_serialized_bytes: usize,
        mut serialized_record_bytes: F,
    ) -> io::Result<RemoteExportReconcileDeltaPreflight>
    where
        F: FnMut(&RemoteExportJournalEntry) -> io::Result<usize>,
    {
        self.validate_fence()?;
        validate_desired_records(desired)?;
        if maximum_entries == 0
            || maximum_record_serialized_bytes == 0
            || maximum_serialized_bytes == 0
            || initial_serialized_bytes > maximum_serialized_bytes
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "remote reconcile delta preflight limits are invalid",
            ));
        }

        let current = self.state()?;
        if cursor.generation != current.delta_generation {
            return Ok(RemoteExportReconcileDeltaPreflight::CursorExpired(
                RemoteDeltaCursorExpired {
                    reason: RemoteDeltaCursorExpiryReason::GenerationMismatch,
                    requested: cursor,
                    current_generation: current.delta_generation,
                    retention_floor: current.retention_floor,
                    through_sequence: current.current_sequence,
                },
            ));
        }
        if cursor.sequence < current.retention_floor {
            return Ok(RemoteExportReconcileDeltaPreflight::CursorExpired(
                RemoteDeltaCursorExpired {
                    reason: RemoteDeltaCursorExpiryReason::Retention,
                    requested: cursor,
                    current_generation: current.delta_generation,
                    retention_floor: current.retention_floor,
                    through_sequence: current.current_sequence,
                },
            ));
        }
        if cursor.sequence > current.current_sequence {
            return Err(invalid_data(
                "remote export cursor is ahead of the durable journal",
            ));
        }

        let mut preview = current.clone();
        if !materialized_set_matches(current, desired)?
            || reconcile_maintenance_due(current, observed_at, self.store)
        {
            reconcile_and_compact(&mut preview, observed_at, desired, mode, self.store)?;
        }

        if cursor.generation != preview.delta_generation {
            return Ok(RemoteExportReconcileDeltaPreflight::CursorExpired(
                RemoteDeltaCursorExpired {
                    reason: RemoteDeltaCursorExpiryReason::GenerationMismatch,
                    requested: cursor,
                    current_generation: preview.delta_generation,
                    retention_floor: preview.retention_floor,
                    through_sequence: preview.current_sequence,
                },
            ));
        }
        if cursor.sequence < preview.retention_floor {
            return Ok(RemoteExportReconcileDeltaPreflight::CursorExpired(
                RemoteDeltaCursorExpired {
                    reason: RemoteDeltaCursorExpiryReason::Retention,
                    requested: cursor,
                    current_generation: preview.delta_generation,
                    retention_floor: preview.retention_floor,
                    through_sequence: preview.current_sequence,
                },
            ));
        }
        if cursor.sequence > preview.current_sequence {
            return Err(invalid_data(
                "remote export cursor is ahead of the reconciled preview",
            ));
        }

        let mut entries = 0usize;
        let mut serialized_bytes = initial_serialized_bytes;
        let start = preview
            .entries
            .partition_point(|entry| entry.sequence <= cursor.sequence);
        for entry in &preview.entries[start..] {
            entries = entries
                .checked_add(1)
                .ok_or_else(|| invalid_data("remote reconcile delta entry count overflowed"))?;
            if entries > maximum_entries {
                return Ok(RemoteExportReconcileDeltaPreflight::LimitExceeded);
            }
            let record_bytes = serialized_record_bytes(entry)?;
            if record_bytes > maximum_record_serialized_bytes {
                return Ok(RemoteExportReconcileDeltaPreflight::LimitExceeded);
            }
            serialized_bytes = match serialized_bytes.checked_add(record_bytes) {
                Some(total) if total <= maximum_serialized_bytes => total,
                _ => return Ok(RemoteExportReconcileDeltaPreflight::LimitExceeded),
            };
        }

        Ok(RemoteExportReconcileDeltaPreflight::Ready {
            cursor: preview.cursor(),
            entries,
            serialized_bytes,
        })
    }

    pub fn read_page(
        &self,
        cursor: DeltaCursor,
        maximum_entries: usize,
        maximum_bytes: u64,
    ) -> io::Result<RemoteDeltaPageRead> {
        self.validate_fence()?;
        let state = self.state()?;
        if cursor.generation != state.delta_generation {
            return Ok(RemoteDeltaPageRead::CursorExpired(
                RemoteDeltaCursorExpired {
                    reason: RemoteDeltaCursorExpiryReason::GenerationMismatch,
                    requested: cursor,
                    current_generation: state.delta_generation,
                    retention_floor: state.retention_floor,
                    through_sequence: state.current_sequence,
                },
            ));
        }
        if cursor.sequence < state.retention_floor {
            return Ok(RemoteDeltaPageRead::CursorExpired(
                RemoteDeltaCursorExpired {
                    reason: RemoteDeltaCursorExpiryReason::Retention,
                    requested: cursor,
                    current_generation: state.delta_generation,
                    retention_floor: state.retention_floor,
                    through_sequence: state.current_sequence,
                },
            ));
        }
        if cursor.sequence > state.current_sequence {
            return Err(invalid_data(
                "remote export cursor is ahead of the durable journal",
            ));
        }
        if maximum_entries == 0
            || maximum_entries > self.store.limits.maximum_page_entries
            || maximum_bytes == 0
            || maximum_bytes > self.store.limits.maximum_page_bytes
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "remote export page limits are outside the supported bounds",
            ));
        }

        let first_sequence = cursor.sequence.saturating_add(1);
        let offset = state
            .entries
            .partition_point(|entry| entry.sequence < first_sequence);
        let mut entries = Vec::new();
        let mut encoded_bytes = 0_u64;
        for entry in state.entries.iter().skip(offset) {
            if entries.len() == maximum_entries {
                break;
            }
            let entry_bytes = encoded_json_size(entry)?;
            let next_bytes = encoded_bytes
                .checked_add(entry_bytes)
                .and_then(|total| total.checked_add(u64::from(!entries.is_empty())))
                .ok_or_else(|| invalid_data("remote export page size overflowed"))?;
            if next_bytes > maximum_bytes {
                if entries.is_empty() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "remote export page byte limit is smaller than its next change",
                    ));
                }
                break;
            }
            encoded_bytes = next_bytes;
            entries.push(entry.clone());
        }

        let through_sequence = entries
            .last()
            .map_or(cursor.sequence, RemoteExportJournalEntry::sequence);
        let next_cursor = DeltaCursor {
            generation: state.delta_generation,
            sequence: through_sequence,
        };
        Ok(RemoteDeltaPageRead::Page(RemoteExportDeltaPage {
            generation: state.delta_generation,
            from_sequence: entries
                .first()
                .map_or(cursor.sequence, RemoteExportJournalEntry::sequence),
            through_sequence,
            next_cursor,
            has_more: through_sequence < state.current_sequence,
            entries,
        }))
    }

    fn state(&self) -> io::Result<&StoredExportState> {
        self.state
            .as_ref()
            .ok_or_else(|| invalid_data("remote export session is poisoned"))
    }

    fn take_state(&mut self) -> io::Result<StoredExportState> {
        self.state
            .take()
            .ok_or_else(|| invalid_data("remote export session is poisoned"))
    }

    fn restore_after_failed_mutation(&mut self) -> io::Result<()> {
        match self.store.read_state() {
            Ok(state) => {
                self.encoded_bytes = encoded_state_size(&state)?;
                self.state = Some(state);
                Ok(())
            }
            Err(error) => {
                self.state = None;
                Err(invalid_data(format!(
                    "remote export mutation failed and durable state could not be reloaded: {error}"
                )))
            }
        }
    }

    fn validate_fence(&self) -> io::Result<()> {
        self.store.validate_namespace()?;
        validate_opened_private_file(
            &self.store.source_directory().join(EXPORT_LOCK_FILE),
            &self.lock,
            "remote export lock",
        )
    }
}

fn preserve_unavailable_git_evidence(
    current: Option<&StoredLiveState>,
    mut incoming: Vec<RemoteProjectDescriptor>,
) -> Vec<RemoteProjectDescriptor> {
    let Some(current) = current else {
        return incoming;
    };
    let current = current
        .project_descriptors
        .iter()
        .map(|descriptor| {
            (
                descriptor.observed_project_key.as_str(),
                &descriptor.git_evidence,
            )
        })
        .collect::<BTreeMap<_, _>>();
    for descriptor in &mut incoming {
        if matches!(
            &descriptor.git_evidence,
            RemoteGitRepositoryEvidence::Unavailable
        ) && let Some(evidence) = current.get(descriptor.observed_project_key.as_str())
        {
            descriptor.git_evidence = (*evidence).clone();
        }
    }
    incoming
}

fn materialized_set_matches(
    state: &StoredExportState,
    desired: &[RemoteExportDesiredRecord],
) -> io::Result<bool> {
    if state.materialized_records.len() != desired.len() {
        return Ok(false);
    }
    let current_upserts = capture_materialized_upserts(state)?;
    Ok(state
        .materialized_records
        .iter()
        .zip(desired)
        .all(|(current, wanted)| {
            current.logical_key == wanted.logical_key
                && current.expires_at == wanted.expires_at
                && current.current_change_id == wanted.upsert.change_id
                && current.tombstone == wanted.tombstone
                && current_upserts
                    .get(&current.logical_key)
                    .is_some_and(|upsert| upsert == &wanted.upsert)
        }))
}

fn reconcile_maintenance_due(
    state: &StoredExportState,
    observed_at: DateTime<Utc>,
    store: &RemoteExportStateStore,
) -> bool {
    if store.limits.retention_days <= 0 {
        return true;
    }

    // Do not delay an expiry which lies within the retention clock's trusted
    // forward window. Suspicious larger clock jumps still enter the existing
    // persisted confirmation path instead of deleting records immediately.
    let trusted_forward_limit = state
        .retention_clock
        .trusted_at
        .checked_add_signed(Duration::hours(
            RETENTION_CLOCK_MAX_UNCONFIRMED_FORWARD_HOURS,
        ))
        .unwrap_or(DateTime::<Utc>::MAX_UTC);
    if observed_at <= trusted_forward_limit
        && state
            .materialized_records
            .iter()
            .any(|record| record.expires_at <= observed_at)
    {
        return true;
    }

    let last_maintenance = state
        .retention_clock
        .pending_last_at
        .unwrap_or(state.retention_clock.trusted_at);
    if state.retention_clock.pending_last_at.is_some() && observed_at < last_maintenance {
        // A corrected wall clock must reach next_retention_clock so the stale
        // future confirmation is cleared instead of freezing maintenance
        // behind an unreachable process-local deadline.
        return true;
    }
    last_maintenance
        .checked_add_signed(Duration::hours(RECONCILE_MAINTENANCE_INTERVAL_HOURS))
        .is_some_and(|next| observed_at >= next)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredExportState {
    format_version: u32,
    source: SourceGeneration,
    redaction_profile: RedactionProfile,
    delta_generation: NonZeroU64,
    current_sequence: u64,
    retention_floor: u64,
    retention_clock: RetentionClock,
    entries: Vec<RemoteExportJournalEntry>,
    materialized_records: Vec<StoredMaterializedRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    live: Option<StoredLiveState>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredLiveState {
    revision: NonZeroU64,
    snapshot: RemoteLiveSnapshot,
    project_descriptors: Vec<RemoteProjectDescriptor>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    partial_reasons: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    warnings: Vec<RemoteDeltaWarning>,
}

impl StoredLiveState {
    fn page(&self, include_snapshot: bool) -> RemoteExportLivePage {
        RemoteExportLivePage {
            state: RemoteLiveState {
                live_revision: self.revision,
                snapshot: include_snapshot.then(|| self.snapshot.clone()),
            },
            project_descriptors: if include_snapshot {
                self.project_descriptors.clone()
            } else {
                Vec::new()
            },
            partial_reasons: self.partial_reasons.clone(),
            warnings: self.warnings.clone(),
        }
    }
}

impl StoredExportState {
    fn new(
        source: SourceGeneration,
        redaction_profile: RedactionProfile,
        observed_at: DateTime<Utc>,
    ) -> io::Result<Self> {
        Ok(Self {
            format_version: STATE_FORMAT_VERSION,
            source,
            redaction_profile,
            delta_generation: generate_delta_generation()?,
            current_sequence: 0,
            retention_floor: 0,
            retention_clock: RetentionClock::current(observed_at),
            entries: Vec::new(),
            materialized_records: Vec::new(),
            live: None,
        })
    }

    fn cursor(&self) -> DeltaCursor {
        DeltaCursor {
            generation: self.delta_generation,
            sequence: self.current_sequence,
        }
    }

    fn validate(&self, store: &RemoteExportStateStore) -> io::Result<()> {
        if self.format_version != STATE_FORMAT_VERSION {
            return Err(invalid_data(format!(
                "unsupported remote export state format {}; expected {}",
                self.format_version, STATE_FORMAT_VERSION
            )));
        }
        if self.source != store.source || self.redaction_profile != store.redaction_profile {
            return Err(invalid_data(
                "remote export state binding does not match its namespace",
            ));
        }
        self.retention_clock.validate()?;
        if let Some(live) = &self.live {
            validate_live_parts(
                self.redaction_profile,
                live.snapshot.captured_at,
                &live.snapshot,
                &live.project_descriptors,
            )?;
            validate_live_quality(&live.partial_reasons, &live.warnings)?;
        }
        if self.retention_floor > self.current_sequence
            || self.entries.len() > store.limits.maximum_entries
            || self.materialized_records.len() > MAX_MATERIALIZED_RECORDS
        {
            return Err(invalid_data("remote export journal bounds are invalid"));
        }
        if self.entries.is_empty() {
            if self.retention_floor != self.current_sequence
                || !self.materialized_records.is_empty()
            {
                return Err(invalid_data(
                    "empty remote export journal has an invalid floor or materialized set",
                ));
            }
            return Ok(());
        }

        let expected_first = self
            .retention_floor
            .checked_add(1)
            .ok_or_else(|| invalid_data("remote export retention floor cannot advance"))?;
        if self.entries.first().map(|entry| entry.sequence) != Some(expected_first)
            || self.entries.last().map(|entry| entry.sequence) != Some(self.current_sequence)
        {
            return Err(invalid_data(
                "remote export journal does not cover its declared sequence range",
            ));
        }
        let mut previous_sequence = self.retention_floor;
        let mut previous_committed_at = DateTime::<Utc>::MIN_UTC;
        let mut ids = HashMap::with_capacity(self.entries.len());
        for entry in &self.entries {
            entry.change.validate()?;
            let expected_sequence = previous_sequence
                .checked_add(1)
                .ok_or_else(|| invalid_data("remote export journal sequence overflowed"))?;
            if entry.sequence != expected_sequence || entry.committed_at < previous_committed_at {
                return Err(invalid_data(
                    "remote export journal order or sequence is invalid",
                ));
            }
            if let Some(existing) = ids.insert(
                entry.change.change_id.as_str(),
                entry.change.payload.as_slice(),
            ) && existing != entry.change.payload.as_slice()
            {
                return Err(invalid_data(
                    "remote export journal reuses a change ID with different payload",
                ));
            }
            previous_sequence = entry.sequence;
            previous_committed_at = entry.committed_at;
        }
        self.validate_materialized_records(&ids)?;
        Ok(())
    }

    fn validate_materialized_records(&self, journal_ids: &HashMap<&str, &[u8]>) -> io::Result<()> {
        let mut previous_key: Option<&str> = None;
        let mut owners = HashMap::<&str, (&str, &'static str)>::new();
        for record in &self.materialized_records {
            validate_materialized_key(&record.logical_key)?;
            validate_change_id(&record.current_change_id)?;
            record.tombstone.validate()?;
            if previous_key.is_some_and(|key| key >= record.logical_key.as_str()) {
                return Err(invalid_data(
                    "remote materialized records are not sorted and unique by logical key",
                ));
            }
            previous_key = Some(&record.logical_key);
            if record.current_change_id == record.tombstone.change_id {
                return Err(invalid_data(
                    "remote materialized upsert and tombstone change IDs collide",
                ));
            }
            validate_materialized_id_owner(
                &mut owners,
                &record.current_change_id,
                &record.logical_key,
                "upsert",
            )?;
            validate_materialized_id_owner(
                &mut owners,
                &record.tombstone.change_id,
                &record.logical_key,
                "tombstone",
            )?;
            if record.last_upsert_sequence.get() <= self.retention_floor {
                return Err(invalid_data(
                    "remote materialized upsert lies at or before the retention floor",
                ));
            }
            let entry = self
                .entry_at_sequence(record.last_upsert_sequence.get())
                .ok_or_else(|| {
                    invalid_data("remote materialized upsert sequence is not retained")
                })?;
            if entry.change.change_id != record.current_change_id {
                return Err(invalid_data(
                    "remote materialized upsert sequence does not match its content ID",
                ));
            }
            if self.entries.iter().any(|entry| {
                entry.sequence > record.last_upsert_sequence.get()
                    && entry.change.change_id == record.tombstone.change_id
            }) {
                return Err(invalid_data(
                    "remote materialized current record has a later retained tombstone",
                ));
            }
            if let Some(payload) = journal_ids.get(record.tombstone.change_id.as_str())
                && *payload != record.tombstone.payload.as_slice()
            {
                return Err(invalid_data(
                    "remote materialized tombstone change ID conflicts with journal content",
                ));
            }
        }
        Ok(())
    }

    fn entry_at_sequence(&self, sequence: u64) -> Option<&RemoteExportJournalEntry> {
        let offset = sequence.checked_sub(self.retention_floor)?.checked_sub(1)?;
        let index = usize::try_from(offset).ok()?;
        self.entries
            .get(index)
            .filter(|entry| entry.sequence == sequence)
    }
}

fn validate_live_parts(
    redaction_profile: RedactionProfile,
    observed_at: DateTime<Utc>,
    snapshot: &RemoteLiveSnapshot,
    project_descriptors: &[RemoteProjectDescriptor],
) -> io::Result<()> {
    if snapshot.captured_at > observed_at {
        return Err(invalid_data(
            "remote live capture time follows its durable observation",
        ));
    }
    snapshot
        .validate_for_storage(redaction_profile)
        .map_err(|error| invalid_data(format!("remote live snapshot is invalid: {error}")))?;
    if project_descriptors.len() > MAX_LIVE_PROJECT_DESCRIPTORS {
        return Err(invalid_data(
            "remote live project descriptor count exceeds its bound",
        ));
    }
    let mut previous = None;
    for descriptor in project_descriptors {
        descriptor
            .validate_for_storage()
            .map_err(|error| invalid_data(format!("remote live descriptor is invalid: {error}")))?;
        let key = descriptor.observed_project_key.as_str();
        if previous.is_some_and(|previous: &str| previous >= key) {
            return Err(invalid_data(
                "remote live project descriptors must be sorted and unique",
            ));
        }
        previous = Some(key);
    }
    let described = project_descriptors
        .iter()
        .map(|descriptor| descriptor.observed_project_key.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    let referenced = snapshot
        .tasks
        .iter()
        .filter_map(|task| task.observed_project_key.as_ref())
        .map(|key| key.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    if described != referenced {
        return Err(invalid_data(
            "remote live descriptors do not exactly match snapshot project references",
        ));
    }
    Ok(())
}

fn validate_live_quality(
    partial_reasons: &[String],
    warnings: &[RemoteDeltaWarning],
) -> io::Result<()> {
    validate_remote_partial_reasons_for_storage(partial_reasons).map_err(|error| {
        invalid_data(format!("remote live partial reasons are invalid: {error}"))
    })?;
    if warnings.len() > 128 {
        return Err(invalid_data("remote live warning count exceeds its bound"));
    }
    for warning in warnings {
        warning
            .validate_for_storage()
            .map_err(|error| invalid_data(format!("remote live warning is invalid: {error}")))?;
    }
    if warnings
        .windows(2)
        .any(|warnings| warnings[0].code >= warnings[1].code)
    {
        return Err(invalid_data(
            "remote live warnings must be sorted and unique by code",
        ));
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredMaterializedRecord {
    logical_key: String,
    expires_at: DateTime<Utc>,
    current_change_id: String,
    last_upsert_sequence: NonZeroU64,
    tombstone: RemoteExportChange,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredExportAnchor {
    format_version: u32,
    source: SourceGeneration,
    redaction_profile: RedactionProfile,
    delta_generation: NonZeroU64,
}

impl StoredExportAnchor {
    fn from_state(state: &StoredExportState) -> Self {
        Self {
            format_version: ANCHOR_FORMAT_VERSION,
            source: state.source.clone(),
            redaction_profile: state.redaction_profile,
            delta_generation: state.delta_generation,
        }
    }

    fn validate(&self, store: &RemoteExportStateStore) -> io::Result<()> {
        if self.format_version != ANCHOR_FORMAT_VERSION
            || self.source != store.source
            || self.redaction_profile != store.redaction_profile
        {
            return Err(invalid_data(
                "remote export anchor binding or format is invalid",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RetentionClock {
    trusted_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pending_started_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pending_last_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pending_confirmations: u32,
}

impl RetentionClock {
    fn current(trusted_at: DateTime<Utc>) -> Self {
        Self {
            trusted_at,
            pending_started_at: None,
            pending_last_at: None,
            pending_confirmations: 0,
        }
    }

    fn pending(trusted_at: DateTime<Utc>, observed_at: DateTime<Utc>) -> Self {
        Self {
            trusted_at,
            pending_started_at: Some(observed_at),
            pending_last_at: Some(observed_at),
            pending_confirmations: 1,
        }
    }

    fn validate(self) -> io::Result<()> {
        match (self.pending_started_at, self.pending_last_at) {
            (None, None) if self.pending_confirmations == 0 => Ok(()),
            (Some(started_at), Some(last_at))
                if self.pending_confirmations > 0
                    && started_at <= last_at
                    && self.trusted_at < started_at =>
            {
                Ok(())
            }
            _ => Err(invalid_data("remote export retention clock is invalid")),
        }
    }
}

fn next_retention_clock(
    mut clock: RetentionClock,
    observed_at: DateTime<Utc>,
) -> (RetentionClock, bool) {
    if observed_at <= clock.trusted_at {
        return (RetentionClock::current(clock.trusted_at), false);
    }
    let maximum = clock
        .trusted_at
        .checked_add_signed(Duration::hours(
            RETENTION_CLOCK_MAX_UNCONFIRMED_FORWARD_HOURS,
        ))
        .unwrap_or(DateTime::<Utc>::MAX_UTC);
    if observed_at <= maximum {
        return (RetentionClock::current(observed_at), false);
    }
    let continues = clock
        .pending_last_at
        .is_some_and(|last_at| observed_at >= last_at);
    if !continues {
        return (RetentionClock::pending(clock.trusted_at, observed_at), true);
    }
    clock.pending_last_at = Some(observed_at);
    clock.pending_confirmations = clock.pending_confirmations.saturating_add(1);
    let stable_since = clock
        .pending_started_at
        .and_then(|started_at| {
            started_at.checked_add_signed(Duration::hours(RETENTION_CLOCK_CONFIRMATION_HOURS))
        })
        .unwrap_or(DateTime::<Utc>::MAX_UTC);
    if observed_at >= stable_since
        && clock.pending_confirmations >= RETENTION_CLOCK_MIN_CONFIRMATIONS
    {
        (RetentionClock::current(observed_at), false)
    } else {
        (clock, true)
    }
}

fn append_and_compact(
    state: &mut StoredExportState,
    observed_at: DateTime<Utc>,
    changes: &[RemoteExportChange],
    store: &RemoteExportStateStore,
) -> io::Result<RemoteExportAppendReport> {
    state.validate(store)?;
    let mut current_upserts = capture_materialized_upserts(state)?;
    let append = append_change_batch(state, observed_at, changes, RetainedDuplicatePolicy::Skip)?;
    let (age_count, retention_deferred) =
        advance_retention_and_prune_age(state, observed_at, store.limits.retention_days)?;
    let trusted_at = state.retention_clock.trusted_at;
    let expired_records =
        expire_materialized_records(state, observed_at, trusted_at, &mut current_upserts)?;
    let before_capacity = state.entries.len();
    compact_to_limits(state, store)?;
    let mut pruned_by_capacity = before_capacity.saturating_sub(state.entries.len());
    let materialized_refreshes = refresh_materialized_bootstrap_if_needed(
        state,
        observed_at,
        &current_upserts,
        store,
        &mut pruned_by_capacity,
    )?;
    state.validate(store)?;
    Ok(RemoteExportAppendReport {
        cursor: state.cursor(),
        appended: append
            .sequences
            .len()
            .checked_add(expired_records)
            .ok_or_else(|| invalid_data("remote export append report overflowed"))?,
        duplicates: append.duplicates,
        pruned_by_age: age_count,
        pruned_by_capacity,
        retention_floor: state.retention_floor,
        materialized_refreshes,
        expired_records,
        retention_deferred,
    })
}

fn reconcile_and_compact(
    state: &mut StoredExportState,
    observed_at: DateTime<Utc>,
    desired: &[RemoteExportDesiredRecord],
    mode: RemoteExportReconcileMode,
    store: &RemoteExportStateStore,
) -> io::Result<RemoteExportReconcileReport> {
    state.validate(store)?;
    let previous_upserts = capture_materialized_upserts(state)?;
    validate_reconcile_change_ownership(&state.materialized_records, &previous_upserts, desired)?;
    let (pruned_by_age, retention_deferred) =
        advance_retention_and_prune_age(state, observed_at, store.limits.retention_days)?;
    let trusted_at = state.retention_clock.trusted_at;

    let mut transitions = Vec::new();
    let mut pending = Vec::new();
    let mut old_index = 0;
    let mut desired_index = 0;
    while old_index < state.materialized_records.len() || desired_index < desired.len() {
        let old = state.materialized_records.get(old_index);
        let next = desired.get(desired_index);
        match (old, next) {
            (Some(old), Some(next)) => match old.logical_key.as_str().cmp(next.logical_key()) {
                std::cmp::Ordering::Less => {
                    if old.expires_at <= trusted_at {
                        merge_expired_old_record(old, &mut transitions);
                    } else {
                        merge_omitted_old_record(
                            old,
                            &previous_upserts,
                            mode,
                            state,
                            &mut transitions,
                            &mut pending,
                        )?;
                    }
                    old_index += 1;
                }
                std::cmp::Ordering::Equal => {
                    if old.expires_at <= trusted_at {
                        merge_expired_old_record(old, &mut transitions);
                    } else {
                        merge_desired_record(
                            Some(old),
                            next,
                            state,
                            &mut transitions,
                            &mut pending,
                        );
                    }
                    old_index += 1;
                    desired_index += 1;
                }
                std::cmp::Ordering::Greater => {
                    if next.expires_at > trusted_at {
                        merge_desired_record(None, next, state, &mut transitions, &mut pending);
                    }
                    desired_index += 1;
                }
            },
            (Some(old), None) => {
                if old.expires_at <= trusted_at {
                    merge_expired_old_record(old, &mut transitions);
                } else {
                    merge_omitted_old_record(
                        old,
                        &previous_upserts,
                        mode,
                        state,
                        &mut transitions,
                        &mut pending,
                    )?;
                }
                old_index += 1;
            }
            (None, Some(next)) => {
                if next.expires_at > trusted_at {
                    merge_desired_record(None, next, state, &mut transitions, &mut pending);
                }
                desired_index += 1;
            }
            (None, None) => break,
        }
    }

    let transition_changes = transitions
        .iter()
        .map(|transition| transition.change.clone())
        .collect::<Vec<_>>();
    let append = append_change_batch(
        state,
        observed_at,
        &transition_changes,
        RetainedDuplicatePolicy::Append,
    )?;
    if append.sequences.len() != transitions.len() || append.duplicates != 0 {
        return Err(invalid_data(
            "remote materialized transition batch was not appended exactly once",
        ));
    }
    for record in &mut pending {
        if let Some(transition_index) = record.upsert_transition {
            record.stored.last_upsert_sequence = append.sequences[transition_index];
        }
    }

    let mut upserts_appended = transitions
        .iter()
        .filter(|transition| transition.kind == ReconcileTransitionKind::Upsert)
        .count();
    let tombstones_appended = transitions.len().saturating_sub(upserts_appended);
    let expired_records = transitions
        .iter()
        .filter(|transition| transition.kind == ReconcileTransitionKind::ExpiryTombstone)
        .count();
    let mut bootstrap_upserts_appended = transitions
        .iter()
        .filter(|transition| transition.bootstrap)
        .count();
    let active_upserts = pending
        .iter()
        .map(|record| (record.stored.logical_key.clone(), record.upsert.clone()))
        .collect::<BTreeMap<_, _>>();
    state.materialized_records = pending.into_iter().map(|record| record.stored).collect();

    let before_capacity = state.entries.len();
    compact_to_limits(state, store)?;
    let mut pruned_by_capacity = before_capacity.saturating_sub(state.entries.len());
    let refreshed = refresh_materialized_bootstrap_if_needed(
        state,
        observed_at,
        &active_upserts,
        store,
        &mut pruned_by_capacity,
    )?;
    upserts_appended = upserts_appended
        .checked_add(refreshed)
        .ok_or_else(|| invalid_data("remote materialized upsert report overflowed"))?;
    bootstrap_upserts_appended = bootstrap_upserts_appended
        .checked_add(refreshed)
        .ok_or_else(|| invalid_data("remote materialized bootstrap report overflowed"))?;
    state.validate(store)?;
    Ok(RemoteExportReconcileReport {
        cursor: state.cursor(),
        upserts_appended,
        tombstones_appended,
        expired_records,
        bootstrap_upserts_appended,
        current_records: state.materialized_records.len(),
        pruned_by_age,
        pruned_by_capacity,
        retention_floor: state.retention_floor,
        retention_deferred,
    })
}

fn validate_desired_records(desired: &[RemoteExportDesiredRecord]) -> io::Result<()> {
    if desired.len() > MAX_MATERIALIZED_RECORDS {
        return Err(invalid_data(
            "remote desired materialized set exceeds its record bound",
        ));
    }
    let mut previous_key: Option<&str> = None;
    let mut owners = HashMap::<&str, (&str, &'static str)>::new();
    for record in desired {
        record.validate()?;
        if previous_key.is_some_and(|key| key >= record.logical_key()) {
            return Err(invalid_data(
                "remote desired materialized records must be sorted and unique by logical key",
            ));
        }
        previous_key = Some(record.logical_key());
        validate_materialized_id_owner(
            &mut owners,
            record.upsert.change_id(),
            record.logical_key(),
            "upsert",
        )?;
        validate_materialized_id_owner(
            &mut owners,
            record.tombstone.change_id(),
            record.logical_key(),
            "tombstone",
        )?;
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MaterializedChangeRole {
    Upsert,
    Tombstone,
}

fn validate_reconcile_change_ownership(
    old: &[StoredMaterializedRecord],
    old_upserts: &BTreeMap<String, RemoteExportChange>,
    desired: &[RemoteExportDesiredRecord],
) -> io::Result<()> {
    let mut old_owners = HashMap::<&str, (usize, MaterializedChangeRole)>::new();
    for (index, record) in old.iter().enumerate() {
        old_owners.insert(
            &record.current_change_id,
            (index, MaterializedChangeRole::Upsert),
        );
        old_owners.insert(
            record.tombstone.change_id(),
            (index, MaterializedChangeRole::Tombstone),
        );
    }
    for record in desired {
        if let Ok(index) = old
            .binary_search_by(|candidate| candidate.logical_key.as_str().cmp(record.logical_key()))
            && old[index].expires_at != record.expires_at
        {
            return Err(invalid_data(
                "remote materialized record expiry changed for an existing logical key",
            ));
        }
        for (change, role) in [
            (&record.upsert, MaterializedChangeRole::Upsert),
            (&record.tombstone, MaterializedChangeRole::Tombstone),
        ] {
            let Some((old_index, old_role)) = old_owners.get(change.change_id()).copied() else {
                continue;
            };
            let old_record = &old[old_index];
            if old_record.logical_key != record.logical_key || old_role != role {
                return Err(invalid_data(
                    "remote desired change ID conflicts with a different current key or role",
                ));
            }
            let old_change = match role {
                MaterializedChangeRole::Upsert => old_upserts
                    .get(&old_record.logical_key)
                    .ok_or_else(|| invalid_data("remote current upsert payload is missing"))?,
                MaterializedChangeRole::Tombstone => &old_record.tombstone,
            };
            if old_change != change {
                return Err(invalid_data(
                    "remote desired change ID was reused with different payload",
                ));
            }
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RetainedDuplicatePolicy {
    Skip,
    Append,
}

struct AppendBatchResult {
    sequences: Vec<NonZeroU64>,
    duplicates: usize,
}

fn append_change_batch(
    state: &mut StoredExportState,
    observed_at: DateTime<Utc>,
    changes: &[RemoteExportChange],
    retained_duplicate_policy: RetainedDuplicatePolicy,
) -> io::Result<AppendBatchResult> {
    let mut retained = HashMap::<&str, &RemoteExportChange>::new();
    for entry in &state.entries {
        match retained.insert(entry.change.change_id(), &entry.change) {
            Some(existing) if existing != &entry.change => {
                return Err(invalid_data(
                    "remote export journal has a retained change ID collision",
                ));
            }
            _ => {}
        }
    }
    let mut batch = HashMap::<&str, &RemoteExportChange>::with_capacity(changes.len());
    let mut selected = Vec::with_capacity(changes.len());
    let mut duplicates = 0;
    for change in changes {
        change.validate()?;
        if let Some(existing) = retained.get(change.change_id()) {
            if *existing != change {
                return Err(invalid_data(format!(
                    "remote export change ID {} was reused with different payload",
                    change.change_id()
                )));
            }
            if retained_duplicate_policy == RetainedDuplicatePolicy::Skip {
                duplicates += 1;
                continue;
            }
        }
        if let Some(existing) = batch.insert(change.change_id(), change) {
            if existing != change {
                return Err(invalid_data(format!(
                    "remote export append batch reused change ID {} with different payload",
                    change.change_id()
                )));
            }
            if retained_duplicate_policy == RetainedDuplicatePolicy::Skip {
                duplicates += 1;
                continue;
            }
            return Err(invalid_data(
                "remote materialized transition batch repeats one change ID",
            ));
        }
        selected.push(change);
    }
    let selected_count = u64::try_from(selected.len())
        .map_err(|_| invalid_data("remote export append count does not fit sequence space"))?;
    state
        .current_sequence
        .checked_add(selected_count)
        .ok_or_else(|| invalid_data("remote export sequence overflowed"))?;
    let committed_at = state
        .entries
        .last()
        .map_or(observed_at, |entry| entry.committed_at.max(observed_at))
        .max(state.retention_clock.trusted_at);
    let mut sequences = Vec::with_capacity(selected.len());
    for change in selected {
        let sequence = state
            .current_sequence
            .checked_add(1)
            .ok_or_else(|| invalid_data("remote export sequence overflowed"))?;
        state.entries.push(RemoteExportJournalEntry {
            sequence,
            committed_at,
            change: change.clone(),
        });
        state.current_sequence = sequence;
        sequences.push(
            NonZeroU64::new(sequence)
                .ok_or_else(|| invalid_data("remote export appended a zero sequence"))?,
        );
    }
    Ok(AppendBatchResult {
        sequences,
        duplicates,
    })
}

fn advance_retention_and_prune_age(
    state: &mut StoredExportState,
    observed_at: DateTime<Utc>,
    retention_days: i64,
) -> io::Result<(usize, bool)> {
    let (clock, retention_deferred) = next_retention_clock(state.retention_clock, observed_at);
    state.retention_clock = clock;
    let cutoff = state
        .retention_clock
        .trusted_at
        .checked_sub_signed(Duration::days(retention_days))
        .unwrap_or(DateTime::<Utc>::MIN_UTC);
    let age_count = state
        .entries
        .partition_point(|entry| entry.committed_at < cutoff);
    drop_oldest_entries(state, age_count)?;
    Ok((age_count, retention_deferred))
}

fn expire_materialized_records(
    state: &mut StoredExportState,
    observed_at: DateTime<Utc>,
    trusted_at: DateTime<Utc>,
    current_upserts: &mut BTreeMap<String, RemoteExportChange>,
) -> io::Result<usize> {
    let expired = state
        .materialized_records
        .iter()
        .filter(|record| record.expires_at <= trusted_at)
        .map(|record| (record.logical_key.clone(), record.tombstone.clone()))
        .collect::<Vec<_>>();
    if expired.is_empty() {
        return Ok(0);
    }
    let changes = expired
        .iter()
        .map(|(_, tombstone)| tombstone.clone())
        .collect::<Vec<_>>();
    let append = append_change_batch(
        state,
        observed_at,
        &changes,
        RetainedDuplicatePolicy::Append,
    )?;
    if append.sequences.len() != expired.len() || append.duplicates != 0 {
        return Err(invalid_data(
            "remote materialized expiry batch was not appended exactly once",
        ));
    }
    let expired_keys = expired
        .into_iter()
        .map(|(logical_key, _)| logical_key)
        .collect::<std::collections::BTreeSet<_>>();
    state
        .materialized_records
        .retain(|record| !expired_keys.contains(&record.logical_key));
    for logical_key in &expired_keys {
        if current_upserts.remove(logical_key).is_none() {
            return Err(invalid_data(
                "remote expired materialized record has no retained upsert",
            ));
        }
    }
    Ok(expired_keys.len())
}

fn capture_materialized_upserts(
    state: &StoredExportState,
) -> io::Result<BTreeMap<String, RemoteExportChange>> {
    let mut upserts = BTreeMap::new();
    for record in &state.materialized_records {
        let entry = state
            .entry_at_sequence(record.last_upsert_sequence.get())
            .ok_or_else(|| invalid_data("remote materialized upsert is not retained"))?;
        if entry.change.change_id != record.current_change_id {
            return Err(invalid_data(
                "remote materialized upsert sequence has the wrong content ID",
            ));
        }
        upserts.insert(record.logical_key.clone(), entry.change.clone());
    }
    Ok(upserts)
}

fn materialized_bootstrap_is_complete(state: &StoredExportState) -> bool {
    state.materialized_records.iter().all(|record| {
        record.last_upsert_sequence.get() > state.retention_floor
            && state
                .entry_at_sequence(record.last_upsert_sequence.get())
                .is_some_and(|entry| entry.change.change_id == record.current_change_id)
            && !state.entries.iter().any(|entry| {
                entry.sequence > record.last_upsert_sequence.get()
                    && entry.change.change_id == record.tombstone.change_id
            })
    })
}

fn refresh_materialized_bootstrap_if_needed(
    state: &mut StoredExportState,
    observed_at: DateTime<Utc>,
    upserts: &BTreeMap<String, RemoteExportChange>,
    store: &RemoteExportStateStore,
    pruned_by_capacity: &mut usize,
) -> io::Result<usize> {
    if materialized_bootstrap_is_complete(state) {
        return Ok(0);
    }
    if upserts.len() != state.materialized_records.len() {
        return Err(invalid_data(
            "remote materialized bootstrap is missing a current upsert payload",
        ));
    }
    let changes = state
        .materialized_records
        .iter()
        .map(|record| {
            let upsert = upserts.get(&record.logical_key).ok_or_else(|| {
                invalid_data("remote materialized bootstrap key has no upsert payload")
            })?;
            if upsert.change_id != record.current_change_id {
                return Err(invalid_data(
                    "remote materialized bootstrap upsert content ID changed unexpectedly",
                ));
            }
            Ok(upsert.clone())
        })
        .collect::<io::Result<Vec<_>>>()?;
    let append = append_change_batch(
        state,
        observed_at,
        &changes,
        RetainedDuplicatePolicy::Append,
    )?;
    if append.sequences.len() != state.materialized_records.len() || append.duplicates != 0 {
        return Err(invalid_data(
            "remote materialized bootstrap refresh was not appended exactly once",
        ));
    }
    for (record, sequence) in state
        .materialized_records
        .iter_mut()
        .zip(append.sequences.iter().copied())
    {
        record.last_upsert_sequence = sequence;
    }
    let before_capacity = state.entries.len();
    compact_to_limits(state, store)?;
    *pruned_by_capacity = pruned_by_capacity
        .checked_add(before_capacity.saturating_sub(state.entries.len()))
        .ok_or_else(|| invalid_data("remote export capacity prune count overflowed"))?;
    if !materialized_bootstrap_is_complete(state) {
        return Err(invalid_data(
            "remote materialized current set cannot fit inside export retention limits",
        ));
    }
    Ok(append.sequences.len())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReconcileTransitionKind {
    Upsert,
    Tombstone,
    ExpiryTombstone,
}

struct ReconcileTransition {
    kind: ReconcileTransitionKind,
    bootstrap: bool,
    change: RemoteExportChange,
}

struct PendingMaterializedRecord {
    stored: StoredMaterializedRecord,
    upsert: RemoteExportChange,
    upsert_transition: Option<usize>,
}

fn merge_desired_record(
    old: Option<&StoredMaterializedRecord>,
    desired: &RemoteExportDesiredRecord,
    state: &StoredExportState,
    transitions: &mut Vec<ReconcileTransition>,
    pending: &mut Vec<PendingMaterializedRecord>,
) {
    let content_unchanged =
        old.is_some_and(|old| old.current_change_id == desired.upsert.change_id);
    let retained = old.is_some_and(|old| {
        state
            .entry_at_sequence(old.last_upsert_sequence.get())
            .is_some_and(|entry| entry.change.change_id == old.current_change_id)
    });
    let transition = if content_unchanged && retained {
        None
    } else {
        let index = transitions.len();
        transitions.push(ReconcileTransition {
            kind: ReconcileTransitionKind::Upsert,
            bootstrap: content_unchanged,
            change: desired.upsert.clone(),
        });
        Some(index)
    };
    let last_upsert_sequence = old
        .map(|old| old.last_upsert_sequence)
        .unwrap_or(NonZeroU64::MIN);
    pending.push(PendingMaterializedRecord {
        stored: StoredMaterializedRecord {
            logical_key: desired.logical_key.clone(),
            expires_at: desired.expires_at,
            current_change_id: desired.upsert.change_id.clone(),
            last_upsert_sequence,
            tombstone: desired.tombstone.clone(),
        },
        upsert: desired.upsert.clone(),
        upsert_transition: transition,
    });
}

fn merge_expired_old_record(
    old: &StoredMaterializedRecord,
    transitions: &mut Vec<ReconcileTransition>,
) {
    transitions.push(ReconcileTransition {
        kind: ReconcileTransitionKind::ExpiryTombstone,
        bootstrap: false,
        change: old.tombstone.clone(),
    });
}

fn merge_omitted_old_record(
    old: &StoredMaterializedRecord,
    previous_upserts: &BTreeMap<String, RemoteExportChange>,
    mode: RemoteExportReconcileMode,
    state: &StoredExportState,
    transitions: &mut Vec<ReconcileTransition>,
    pending: &mut Vec<PendingMaterializedRecord>,
) -> io::Result<()> {
    if mode == RemoteExportReconcileMode::Authoritative {
        transitions.push(ReconcileTransition {
            kind: ReconcileTransitionKind::Tombstone,
            bootstrap: false,
            change: old.tombstone.clone(),
        });
        return Ok(());
    }
    let upsert = previous_upserts
        .get(&old.logical_key)
        .ok_or_else(|| invalid_data("remote partial reconcile lost an old current upsert"))?
        .clone();
    let retained = state
        .entry_at_sequence(old.last_upsert_sequence.get())
        .is_some_and(|entry| entry.change.change_id == old.current_change_id);
    let transition = if retained {
        None
    } else {
        let index = transitions.len();
        transitions.push(ReconcileTransition {
            kind: ReconcileTransitionKind::Upsert,
            bootstrap: true,
            change: upsert.clone(),
        });
        Some(index)
    };
    pending.push(PendingMaterializedRecord {
        stored: old.clone(),
        upsert,
        upsert_transition: transition,
    });
    Ok(())
}

fn compact_to_limits(
    state: &mut StoredExportState,
    store: &RemoteExportStateStore,
) -> io::Result<()> {
    if state.entries.len() > store.limits.maximum_entries {
        let excess = state.entries.len() - store.limits.maximum_entries;
        drop_oldest_entries(state, excess)?;
    }

    loop {
        let size = encoded_state_size(state)?;
        if size <= store.limits.maximum_state_bytes {
            return Ok(());
        }
        if state.entries.is_empty() {
            return Err(invalid_data(
                "remote export state envelope exceeds its hard byte cap",
            ));
        }
        let excess = size - store.limits.maximum_state_bytes;
        let mut reclaimed = 0_u64;
        let mut count = 0;
        for entry in &state.entries {
            reclaimed = reclaimed
                .checked_add(encoded_json_size(entry)?)
                .and_then(|value| value.checked_add(1))
                .ok_or_else(|| invalid_data("remote export compaction size overflowed"))?;
            count += 1;
            if reclaimed >= excess {
                break;
            }
        }
        drop_oldest_entries(state, count)?;
    }
}

fn drop_oldest_entries(state: &mut StoredExportState, count: usize) -> io::Result<()> {
    if count == 0 {
        return Ok(());
    }
    if count > state.entries.len() {
        return Err(invalid_data(
            "remote export compaction exceeded retained entries",
        ));
    }
    state.retention_floor = state.entries[count - 1].sequence;
    state.entries.drain(..count);
    if state.entries.is_empty() {
        state.retention_floor = state.current_sequence;
    }
    Ok(())
}

fn generate_delta_generation() -> io::Result<NonZeroU64> {
    for _ in 0..16 {
        let mut bytes = [0_u8; 8];
        getrandom::fill(&mut bytes).map_err(|error| {
            io::Error::other(format!(
                "could not generate remote delta journal generation: {error}"
            ))
        })?;
        if let Some(generation) = NonZeroU64::new(u64::from_le_bytes(bytes)) {
            return Ok(generation);
        }
    }
    Err(io::Error::other(
        "secure random provider repeatedly returned a zero delta generation",
    ))
}

fn prepare_private_root(path: &Path) -> io::Result<()> {
    if path.as_os_str().is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "remote export state root must not be empty",
        ));
    }
    match validate_private_directory(path, "remote export state root") {
        Ok(()) => return Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;

        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)?;
    }
    #[cfg(not(unix))]
    fs::create_dir_all(path)?;
    validate_private_directory(path, "remote export state root")
}

fn create_private_child_directory(parent: &Path, path: &Path) -> io::Result<()> {
    validate_private_directory(parent, "remote export parent directory")?;
    match fs::symlink_metadata(path) {
        Ok(_) => validate_private_directory(path, "remote export state directory")?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;

                fs::DirBuilder::new().mode(0o700).create(path)?;
            }
            #[cfg(not(unix))]
            fs::create_dir(path)?;
            validate_private_directory(path, "remote export state directory")?;
            sync_directory(parent)?;
        }
        Err(error) => return Err(error),
    }
    validate_private_directory(parent, "remote export parent directory")
}

fn validate_private_directory(path: &Path, subject: &str) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata_is_link_or_reparse(&metadata) {
        return Err(invalid_data(format!(
            "{subject} must not be a symbolic link or reparse point"
        )));
    }
    if !metadata.file_type().is_dir() {
        return Err(invalid_data(format!("{subject} must be a directory")));
    }
    ensure_private_directory(path, &metadata, subject)
}

fn try_open_source_lock(store: &RemoteExportStateStore, directory: &Path) -> io::Result<File> {
    validate_private_directory(directory, "remote export source directory")?;
    let path = directory.join(EXPORT_LOCK_FILE);
    match fs::symlink_metadata(&path) {
        Ok(metadata) => validate_file_metadata(&path, &metadata, "remote export lock")?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        options.mode(0o600);
    }
    add_nofollow_flags(&mut options);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;

        options.share_mode(stable_lock_share_mode());
    }
    let file = options
        .open(&path)
        .map_err(|error| map_nofollow_error(error, "remote export lock"))?;
    validate_opened_private_file(&path, &file, "remote export lock")?;
    match fs2::FileExt::try_lock_exclusive(&file) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
            validate_opened_private_file(&path, &file, "remote export lock")?;
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "remote exporter is already running for this source generation",
            ));
        }
        Err(error) => return Err(error),
    }
    store.prepare_source_directory()?;
    validate_opened_private_file(&path, &file, "remote export lock")?;
    Ok(file)
}

fn read_private_json_file<T: for<'de> Deserialize<'de>>(
    path: &Path,
    maximum_bytes: u64,
    subject: &str,
) -> io::Result<T> {
    let parent = path
        .parent()
        .ok_or_else(|| invalid_data(format!("{subject} has no parent directory")))?;
    validate_private_directory(parent, "remote export data directory")?;
    let path_metadata = fs::symlink_metadata(path)?;
    validate_file_metadata(path, &path_metadata, subject)?;
    let mut options = OpenOptions::new();
    options.read(true);
    add_nofollow_flags(&mut options);
    let file = options
        .open(path)
        .map_err(|error| map_nofollow_error(error, subject))?;
    let metadata = file.metadata()?;
    validate_file_metadata(path, &metadata, subject)?;
    ensure_opened_file_matches_path(path, &file, &path_metadata, &metadata, subject)?;
    ensure_private_file(path, &file, &metadata, subject)?;
    if metadata.len() > maximum_bytes {
        return Err(invalid_data(format!("{subject} is too large")));
    }
    let capacity = usize::try_from(metadata.len())
        .map_err(|_| invalid_data(format!("{subject} size does not fit in memory")))?;
    let mut contents = Vec::with_capacity(capacity);
    file.take(maximum_bytes.saturating_add(1))
        .read_to_end(&mut contents)?;
    if contents.len() as u64 > maximum_bytes {
        return Err(invalid_data(format!("{subject} is too large")));
    }
    let value = serde_json::from_slice(&contents)
        .map_err(|error| invalid_data(format!("{subject} is invalid: {error}")))?;
    validate_private_directory(parent, "remote export data directory")?;
    Ok(value)
}

fn write_private_json_atomically<T: Serialize>(
    path: &Path,
    value: &T,
    maximum_bytes: u64,
    subject: &str,
) -> io::Result<u64> {
    let parent = path
        .parent()
        .ok_or_else(|| invalid_data(format!("{subject} has no parent directory")))?;
    validate_private_directory(parent, "remote export data directory")?;
    match fs::symlink_metadata(path) {
        Ok(_) => {
            let existing = open_private_file(path, subject)?;
            drop(existing);
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let file_name = path.file_name().unwrap_or_else(|| OsStr::new("export"));
    let (temporary, mut file) = create_temporary_file(parent, file_name, subject)?;
    let result = (|| {
        let written = {
            let mut writer = LimitedWriter::new(&mut file, maximum_bytes);
            serde_json::to_writer(&mut writer, value)
                .map_err(|error| invalid_data(format!("{subject} is invalid: {error}")))?;
            writer.write_all(b"\n")?;
            writer.written()
        };
        file.sync_all()?;
        drop(file);
        replace_file(&temporary, path)?;
        validate_published_private_file(path, subject)?;
        sync_directory(parent)?;
        Ok(written)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn create_private_json_once<T: Serialize>(
    path: &Path,
    value: &T,
    maximum_bytes: u64,
    subject: &str,
) -> io::Result<bool> {
    let parent = path
        .parent()
        .ok_or_else(|| invalid_data(format!("{subject} has no parent directory")))?;
    validate_private_directory(parent, "remote export data directory")?;
    let file_name = path.file_name().unwrap_or_else(|| OsStr::new("export"));
    let (temporary, mut file) = create_temporary_file(parent, file_name, subject)?;
    let result = (|| {
        {
            let mut writer = LimitedWriter::new(&mut file, maximum_bytes);
            serde_json::to_writer(&mut writer, value)
                .map_err(|error| invalid_data(format!("{subject} is invalid: {error}")))?;
            writer.write_all(b"\n")?;
        }
        file.sync_all()?;
        drop(file);
        match fs::hard_link(&temporary, path) {
            Ok(()) => {
                validate_published_private_file(path, subject)?;
                fs::remove_file(&temporary)?;
                sync_directory(parent)?;
                Ok(true)
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                fs::remove_file(&temporary)?;
                Ok(false)
            }
            Err(error) => Err(error),
        }
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn create_temporary_file(
    parent: &Path,
    file_name: &OsStr,
    subject: &str,
) -> io::Result<(PathBuf, File)> {
    validate_private_directory(parent, "remote export data directory")?;
    for _ in 0..TEMP_FILE_ATTEMPTS {
        let sequence = TEMP_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temporary = parent.join(format!(
            ".{}.{}.{}.tmp",
            file_name.to_string_lossy(),
            std::process::id(),
            sequence
        ));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;

            options.mode(0o600);
        }
        add_nofollow_flags(&mut options);
        match options.open(&temporary) {
            Ok(file) => match validate_opened_private_file(&temporary, &file, subject) {
                Ok(()) => return Ok((temporary, file)),
                Err(error) => {
                    drop(file);
                    let _ = fs::remove_file(&temporary);
                    return Err(error);
                }
            },
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a unique remote export temporary file",
    ))
}

fn open_private_file(path: &Path, subject: &str) -> io::Result<File> {
    let path_metadata = fs::symlink_metadata(path)?;
    validate_file_metadata(path, &path_metadata, subject)?;
    let mut options = OpenOptions::new();
    options.read(true);
    add_nofollow_flags(&mut options);
    let file = options
        .open(path)
        .map_err(|error| map_nofollow_error(error, subject))?;
    let metadata = file.metadata()?;
    validate_file_metadata(path, &metadata, subject)?;
    ensure_opened_file_matches_path(path, &file, &path_metadata, &metadata, subject)?;
    ensure_private_file(path, &file, &metadata, subject)?;
    Ok(file)
}

fn validate_published_private_file(path: &Path, subject: &str) -> io::Result<()> {
    let file = open_private_file(path, subject)?;
    drop(file);
    Ok(())
}

fn validate_opened_private_file(path: &Path, file: &File, subject: &str) -> io::Result<()> {
    let path_metadata = fs::symlink_metadata(path)?;
    validate_file_metadata(path, &path_metadata, subject)?;
    let opened_metadata = file.metadata()?;
    validate_file_metadata(path, &opened_metadata, subject)?;
    ensure_opened_file_matches_path(path, file, &path_metadata, &opened_metadata, subject)?;
    ensure_private_file(path, file, &opened_metadata, subject)
}

fn validate_file_metadata(path: &Path, metadata: &fs::Metadata, subject: &str) -> io::Result<()> {
    if metadata_is_link_or_reparse(metadata) {
        return Err(invalid_data(format!(
            "{subject} must not be a symbolic link or reparse point ({})",
            path.display()
        )));
    }
    if !metadata.file_type().is_file() {
        return Err(invalid_data(format!("{subject} must be a regular file")));
    }
    ensure_private_path(metadata, subject)
}

fn add_nofollow_flags(options: &mut OpenOptions) {
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

fn map_nofollow_error(error: io::Error, subject: &str) -> io::Error {
    #[cfg(unix)]
    if error.raw_os_error() == Some(libc::ELOOP) {
        return invalid_data(format!("{subject} must not be a symbolic link"));
    }
    #[cfg(not(unix))]
    let _ = subject;
    error
}

#[cfg(unix)]
fn metadata_is_link_or_reparse(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(windows)]
fn metadata_is_link_or_reparse(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(any(unix, windows)))]
fn metadata_is_link_or_reparse(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(unix)]
fn ensure_private_path(metadata: &fs::Metadata, subject: &str) -> io::Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    // SAFETY: geteuid has no preconditions and retains no pointers.
    let effective_uid = unsafe { libc::geteuid() };
    if metadata.uid() != effective_uid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{subject} must be owned by the current user"),
        ));
    }
    if metadata.permissions().mode() & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{subject} must not be accessible by group or other users"),
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn ensure_private_path(_metadata: &fs::Metadata, _subject: &str) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn ensure_private_directory(
    _path: &Path,
    metadata: &fs::Metadata,
    subject: &str,
) -> io::Result<()> {
    ensure_private_path(metadata, subject)
}

#[cfg(windows)]
fn ensure_private_directory(
    path: &Path,
    _metadata: &fs::Metadata,
    subject: &str,
) -> io::Result<()> {
    crate::source_identity::validate_windows_private_directory(path, subject)
}

#[cfg(not(any(unix, windows)))]
fn ensure_private_directory(
    _path: &Path,
    _metadata: &fs::Metadata,
    _subject: &str,
) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "private remote export directories are unsupported on this platform",
    ))
}

#[cfg(unix)]
fn ensure_private_file(
    _path: &Path,
    _file: &File,
    metadata: &fs::Metadata,
    subject: &str,
) -> io::Result<()> {
    ensure_private_path(metadata, subject)
}

#[cfg(windows)]
fn ensure_private_file(
    path: &Path,
    file: &File,
    _metadata: &fs::Metadata,
    subject: &str,
) -> io::Result<()> {
    crate::source_identity::validate_windows_private_file(path, file, subject)
}

#[cfg(not(any(unix, windows)))]
fn ensure_private_file(
    _path: &Path,
    _file: &File,
    _metadata: &fs::Metadata,
    _subject: &str,
) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "private remote export files are unsupported on this platform",
    ))
}

#[cfg(unix)]
fn ensure_opened_file_matches_path(
    _path: &Path,
    _file: &File,
    path_metadata: &fs::Metadata,
    opened_metadata: &fs::Metadata,
    subject: &str,
) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;

    if path_metadata.dev() == opened_metadata.dev() && path_metadata.ino() == opened_metadata.ino()
    {
        Ok(())
    } else {
        Err(invalid_data(format!(
            "{subject} changed while it was being opened"
        )))
    }
}

#[cfg(windows)]
fn ensure_opened_file_matches_path(
    path: &Path,
    file: &File,
    _path_metadata: &fs::Metadata,
    _opened_metadata: &fs::Metadata,
    subject: &str,
) -> io::Result<()> {
    let expected = windows_file_identity(file, subject)?;
    let mut options = OpenOptions::new();
    options.read(true);
    add_nofollow_flags(&mut options);
    let current = options
        .open(path)
        .map_err(|error| map_nofollow_error(error, subject))?;
    crate::source_identity::validate_windows_private_file(path, &current, subject)?;
    if windows_file_identity(&current, subject)? == expected {
        Ok(())
    } else {
        Err(invalid_data(format!(
            "{subject} changed while it was being opened"
        )))
    }
}

#[cfg(windows)]
fn windows_file_identity(file: &File, subject: &str) -> io::Result<(u32, u64)> {
    use std::mem::MaybeUninit;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };

    let mut information = MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::uninit();
    // SAFETY: the file owns a live handle and the output points to correctly
    // sized uninitialized storage populated on success.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), information.as_mut_ptr()) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the API succeeded and initialized the complete structure.
    let information = unsafe { information.assume_init() };
    let index =
        (u64::from(information.nFileIndexHigh) << 32) | u64::from(information.nFileIndexLow);
    if information.dwVolumeSerialNumber == 0 && index == 0 {
        return Err(invalid_data(format!(
            "{subject} does not expose a stable Windows file identity"
        )));
    }
    Ok((information.dwVolumeSerialNumber, index))
}

#[cfg(not(any(unix, windows)))]
fn ensure_opened_file_matches_path(
    _path: &Path,
    _file: &File,
    _path_metadata: &fs::Metadata,
    _opened_metadata: &fs::Metadata,
    _subject: &str,
) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "stable remote export files are unsupported on this platform",
    ))
}

#[cfg(windows)]
fn stable_lock_share_mode() -> u32 {
    use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};

    FILE_SHARE_READ | FILE_SHARE_WRITE
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[derive(Default)]
struct CountingWriter {
    written: u64,
}

impl Write for CountingWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.written = self
            .written
            .checked_add(buffer.len() as u64)
            .ok_or_else(|| invalid_data("serialized remote export size overflowed"))?;
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct LimitedWriter<'a> {
    inner: &'a mut File,
    maximum: u64,
    written: u64,
}

impl<'a> LimitedWriter<'a> {
    fn new(inner: &'a mut File, maximum: u64) -> Self {
        Self {
            inner,
            maximum,
            written: 0,
        }
    }

    fn written(&self) -> u64 {
        self.written
    }
}

impl Write for LimitedWriter<'_> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let requested = buffer.len() as u64;
        let next = self
            .written
            .checked_add(requested)
            .ok_or_else(|| invalid_data("serialized remote export size overflowed"))?;
        if next > self.maximum {
            return Err(invalid_data(
                "serialized remote export state exceeds its hard byte cap",
            ));
        }
        self.inner.write_all(buffer)?;
        self.written = next;
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

fn encoded_json_size<T: Serialize>(value: &T) -> io::Result<u64> {
    let mut writer = CountingWriter::default();
    serde_json::to_writer(&mut writer, value)
        .map_err(|error| invalid_data(format!("remote export value is invalid: {error}")))?;
    Ok(writer.written)
}

fn encoded_state_size(state: &StoredExportState) -> io::Result<u64> {
    encoded_json_size(state)?
        .checked_add(1)
        .ok_or_else(|| invalid_data("serialized remote export state size overflowed"))
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroU64;

    use chrono::TimeZone;
    use tempfile::tempdir;

    use crate::domain::TaskStatus;
    use crate::remote_protocol::{GitRepositoryFingerprint, RemoteLiveTask, RemoteTokenUsage};

    const NODE_ONE: &str = "node-0123456789abcdef0123456789abcdef";

    fn at(day: u32, hour: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, day, hour, 0, 0)
            .single()
            .unwrap()
    }

    fn source(generation: u64) -> SourceGeneration {
        SourceGeneration {
            node_id: NODE_ONE.parse().unwrap(),
            generation: NonZeroU64::new(generation).unwrap(),
        }
    }

    fn change(id: impl fmt::Display, byte: u8) -> RemoteExportChange {
        RemoteExportChange::new(format!("change-{id}"), vec![byte; 8]).unwrap()
    }

    fn desired(key: &str, version: u8) -> RemoteExportDesiredRecord {
        desired_until(key, version, at(31, 0))
    }

    fn desired_until(
        key: &str,
        version: u8,
        expires_at: DateTime<Utc>,
    ) -> RemoteExportDesiredRecord {
        RemoteExportDesiredRecord::new(
            key,
            expires_at,
            RemoteExportChange::new(format!("upsert-{key}-v{version}"), vec![version; 8]).unwrap(),
            RemoteExportChange::new(format!("tombstone-{key}"), vec![0xf0; 8]).unwrap(),
        )
        .unwrap()
    }

    fn page(read: RemoteDeltaPageRead) -> RemoteExportDeltaPage {
        match read {
            RemoteDeltaPageRead::Page(page) => page,
            RemoteDeltaPageRead::CursorExpired(expired) => {
                panic!("unexpected cursor expiry: {expired:?}")
            }
        }
    }

    fn entry_ids(page: &RemoteExportDeltaPage) -> Vec<&str> {
        page.entries
            .iter()
            .map(|entry| entry.change().change_id())
            .collect()
    }

    #[test]
    fn construction_is_side_effect_free_and_generation_sequence_persist() {
        let directory = tempdir().unwrap();
        let root = directory.path().join("state");
        let store = RemoteExportStateStore::new(&root, source(7), RedactionProfile::PreviewEnabled);
        assert!(!root.exists());

        let first = store.try_begin(at(1, 0)).unwrap().status().unwrap();
        assert_eq!(first.cursor.sequence, 0);
        drop(store.try_begin(at(1, 1)).unwrap());

        let reopened =
            RemoteExportStateStore::new(&root, source(7), RedactionProfile::PreviewEnabled)
                .try_begin(at(1, 2))
                .unwrap()
                .status()
                .unwrap();
        assert_eq!(reopened.cursor, first.cursor);
        assert_eq!(reopened.retention_floor, 0);
    }

    #[test]
    fn live_quality_is_durable_and_can_change_without_bumping_content_revision() {
        let directory = tempdir().unwrap();
        let store = RemoteExportStateStore::new(
            directory.path().join("state"),
            source(1),
            RedactionProfile::Redacted,
        );
        let captured_at = at(1, 0);
        let snapshot = RemoteLiveSnapshot {
            captured_at,
            tasks: Vec::new(),
            turns: Vec::new(),
        };
        let warning = RemoteDeltaWarning {
            code: "live_snapshot_truncated".to_owned(),
            occurrences: NonZeroU64::new(3).unwrap(),
        };

        let mut export = store.try_begin(captured_at).unwrap();
        let first = export
            .reconcile_live_snapshot(
                captured_at,
                snapshot.clone(),
                Vec::new(),
                vec!["live_snapshot_truncated".to_owned()],
                vec![warning.clone()],
                None,
            )
            .unwrap();
        assert_eq!(first.state.live_revision, NonZeroU64::new(1).unwrap());
        assert!(first.state.snapshot.is_some());
        assert_eq!(first.partial_reasons, vec!["live_snapshot_truncated"]);
        assert_eq!(first.warnings, vec![warning.clone()]);

        // A new scan can improve or degrade live quality while the rows stay
        // byte-for-byte identical. The content revision remains stable, but
        // the current response and pending continuations carry the new
        // quality instead of inheriting the center's previous assessment.
        let improved = export
            .reconcile_live_snapshot(
                captured_at + Duration::minutes(1),
                snapshot,
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Some(first.state.live_revision),
            )
            .unwrap();
        assert_eq!(improved.state.live_revision, first.state.live_revision);
        assert!(improved.state.snapshot.is_none());
        assert!(improved.partial_reasons.is_empty());
        assert!(improved.warnings.is_empty());
        drop(export);

        let reopened = store.try_begin(captured_at + Duration::minutes(2)).unwrap();
        let continuation = reopened
            .current_live_page(Some(first.state.live_revision))
            .unwrap()
            .unwrap();
        assert!(continuation.state.snapshot.is_none());
        assert!(continuation.partial_reasons.is_empty());
        assert!(continuation.warnings.is_empty());

        let bootstrap = reopened.current_live_page(None).unwrap().unwrap();
        assert!(bootstrap.state.snapshot.is_some());
        assert_eq!(bootstrap.state.live_revision, first.state.live_revision);
    }

    #[test]
    fn unavailable_git_probe_preserves_live_revision_but_authoritative_results_replace() {
        let directory = tempdir().unwrap();
        let store = RemoteExportStateStore::new(
            directory.path().join("state"),
            source(1),
            RedactionProfile::Redacted,
        );
        let key: crate::source_model::ObservedProjectKey =
            format!("opk-hmac-sha256-v1-{}", "a".repeat(64))
                .parse()
                .unwrap();
        let fingerprint: GitRepositoryFingerprint =
            format!("git-sha256-v1-{}", "b".repeat(64)).parse().unwrap();
        let snapshot = RemoteLiveSnapshot {
            captured_at: at(1, 0),
            tasks: vec![RemoteLiveTask {
                thread_id: "thread-1".parse().unwrap(),
                parent_thread_id: None,
                observed_project_key: Some(key.clone()),
                title_preview: None,
                created_at: Some(at(1, 0)),
                updated_at: at(1, 0),
                status: TaskStatus::Completed,
                token_usage: RemoteTokenUsage::default(),
                turn_count: 0,
            }],
            turns: Vec::new(),
        };
        let descriptor = |git_evidence| RemoteProjectDescriptor {
            observed_project_key: key.clone(),
            display_label: "workspace".parse().unwrap(),
            git_evidence,
        };
        let mut export = store.try_begin(at(1, 0)).unwrap();
        let first = export
            .reconcile_live_snapshot(
                at(1, 0),
                snapshot.clone(),
                vec![descriptor(RemoteGitRepositoryEvidence::Repository {
                    fingerprint: Some(fingerprint),
                    repository_relative_workspace_root: ".".to_owned(),
                })],
                Vec::new(),
                Vec::new(),
                None,
            )
            .unwrap();
        let unavailable = export
            .reconcile_live_snapshot(
                at(1, 1),
                snapshot.clone(),
                vec![descriptor(RemoteGitRepositoryEvidence::Unavailable)],
                Vec::new(),
                Vec::new(),
                Some(first.state.live_revision),
            )
            .unwrap();
        assert_eq!(unavailable.state.live_revision, first.state.live_revision);
        assert!(unavailable.state.snapshot.is_none());

        let non_repository = export
            .reconcile_live_snapshot(
                at(1, 2),
                snapshot.clone(),
                vec![descriptor(
                    RemoteGitRepositoryEvidence::ConfirmedNonRepository,
                )],
                Vec::new(),
                Vec::new(),
                Some(first.state.live_revision),
            )
            .unwrap();
        assert_eq!(
            non_repository.state.live_revision.get(),
            first.state.live_revision.get() + 1
        );
        assert!(non_repository.state.snapshot.is_some());

        let no_origin = export
            .reconcile_live_snapshot(
                at(1, 3),
                snapshot,
                vec![descriptor(RemoteGitRepositoryEvidence::Repository {
                    fingerprint: None,
                    repository_relative_workspace_root: ".".to_owned(),
                })],
                Vec::new(),
                Vec::new(),
                Some(non_repository.state.live_revision),
            )
            .unwrap();
        assert_eq!(
            no_origin.state.live_revision.get(),
            non_repository.state.live_revision.get() + 1
        );
        assert!(no_origin.state.snapshot.is_some());
    }

    #[test]
    fn fixed_old_cursor_retry_is_identical_and_no_change_is_an_explicit_empty_page() {
        let directory = tempdir().unwrap();
        let store = RemoteExportStateStore::new(
            directory.path().join("state"),
            source(1),
            RedactionProfile::Redacted,
        );
        let mut export = store.try_begin(at(1, 0)).unwrap();
        let initial = export.status().unwrap().cursor;
        let changes = [change("a", 1), change("b", 2), change("c", 3)];
        let report = export.append_changes(at(1, 1), &changes).unwrap();
        assert_eq!(report.appended, 3);
        assert_eq!(report.duplicates, 0);
        assert_eq!(report.cursor.sequence, 3);

        let first = page(export.read_page(initial, 2, 4096).unwrap());
        let retried = page(export.read_page(initial, 2, 4096).unwrap());
        assert_eq!(retried, first);
        assert_eq!(first.from_sequence, 1);
        assert_eq!(first.through_sequence, 2);
        assert!(first.has_more);

        let tail = page(export.read_page(first.next_cursor, 2, 4096).unwrap());
        assert_eq!(tail.from_sequence, 3);
        assert_eq!(tail.through_sequence, 3);
        assert!(!tail.has_more);
        let empty = page(export.read_page(tail.next_cursor, 2, 4096).unwrap());
        assert!(empty.entries.is_empty());
        assert_eq!(empty.from_sequence, 3);
        assert_eq!(empty.through_sequence, 3);
        assert_eq!(empty.next_cursor, tail.next_cursor);
        assert!(!empty.has_more);

        let duplicate = export
            .append_changes(at(1, 2), std::slice::from_ref(&changes[1]))
            .unwrap();
        assert_eq!(duplicate.appended, 0);
        assert_eq!(duplicate.duplicates, 1);
        assert_eq!(duplicate.cursor.sequence, 3);
    }

    #[test]
    fn conflicting_idempotency_key_fails_without_advancing_durable_state() {
        let directory = tempdir().unwrap();
        let store = RemoteExportStateStore::new(
            directory.path().join("state"),
            source(1),
            RedactionProfile::Redacted,
        );
        let mut export = store.try_begin(at(1, 0)).unwrap();
        export
            .append_changes(at(1, 1), &[change("same", 1)])
            .unwrap();
        let before = export.status().unwrap();
        let error = export
            .append_changes(at(1, 2), &[change("same", 2)])
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(export.status().unwrap(), before);
        drop(export);
        assert_eq!(store.try_begin(at(1, 3)).unwrap().status().unwrap(), before);
    }

    #[test]
    fn cursor_generation_mismatch_and_retention_floor_are_explicitly_expired() {
        let directory = tempdir().unwrap();
        let limits = RemoteExportLimits {
            maximum_entries: 2,
            ..RemoteExportLimits::default()
        };
        let store = RemoteExportStateStore::new(
            directory.path().join("state"),
            source(1),
            RedactionProfile::Redacted,
        )
        .with_limits(limits);
        let mut export = store.try_begin(at(1, 0)).unwrap();
        let old = export.status().unwrap().cursor;
        let report = export
            .append_changes(
                at(1, 1),
                &[change("one", 1), change("two", 2), change("three", 3)],
            )
            .unwrap();
        assert_eq!(report.pruned_by_capacity, 1);
        assert_eq!(report.retention_floor, 1);

        let expired = export.read_page(old, 10, 4096).unwrap();
        assert!(matches!(
            expired,
            RemoteDeltaPageRead::CursorExpired(RemoteDeltaCursorExpired {
                reason: RemoteDeltaCursorExpiryReason::Retention,
                retention_floor: 1,
                through_sequence: 3,
                ..
            })
        ));
        let wrong_generation = DeltaCursor {
            generation: NonZeroU64::new(old.generation.get().wrapping_add(1))
                .unwrap_or(NonZeroU64::MIN),
            sequence: report.cursor.sequence,
        };
        assert!(matches!(
            export.read_page(wrong_generation, 10, 4096).unwrap(),
            RemoteDeltaPageRead::CursorExpired(RemoteDeltaCursorExpired {
                reason: RemoteDeltaCursorExpiryReason::GenerationMismatch,
                ..
            })
        ));
        let retained = page(
            export
                .read_page(
                    DeltaCursor {
                        generation: report.cursor.generation,
                        sequence: report.retention_floor,
                    },
                    10,
                    4096,
                )
                .unwrap(),
        );
        assert_eq!(
            retained
                .entries
                .iter()
                .map(RemoteExportJournalEntry::sequence)
                .collect::<Vec<_>>(),
            vec![2, 3]
        );
    }

    #[test]
    fn byte_cap_prunes_oldest_entries_and_advances_the_same_expiry_floor() {
        let directory = tempdir().unwrap();
        let root = directory.path().join("state");
        let base_store = RemoteExportStateStore::new(&root, source(1), RedactionProfile::Redacted);
        drop(base_store.try_begin(at(1, 0)).unwrap());
        let mut one_entry = base_store.read_state().unwrap();
        one_entry.entries.push(RemoteExportJournalEntry {
            sequence: 1,
            committed_at: at(1, 1),
            change: change("a", 1),
        });
        one_entry.current_sequence = 1;
        let one_entry_limit = encoded_state_size(&one_entry).unwrap();
        let store = base_store.with_limits(RemoteExportLimits {
            maximum_state_bytes: one_entry_limit,
            ..RemoteExportLimits::default()
        });
        let mut export = store.try_begin(at(1, 1)).unwrap();
        let old = export.status().unwrap().cursor;
        let report = export
            .append_changes(at(1, 1), &[change("a", 1), change("b", 2), change("c", 3)])
            .unwrap();
        assert_eq!(report.pruned_by_capacity, 2);
        assert_eq!(report.retention_floor, 2);
        let status = export.status().unwrap();
        assert_eq!(status.retained_entries, 1);
        assert!(status.encoded_bytes <= one_entry_limit);
        assert!(matches!(
            export.read_page(old, 10, 4096).unwrap(),
            RemoteDeltaPageRead::CursorExpired(RemoteDeltaCursorExpired {
                reason: RemoteDeltaCursorExpiryReason::Retention,
                retention_floor: 2,
                ..
            })
        ));
    }

    #[test]
    fn thirty_five_day_retention_expires_only_cursors_before_the_floor() {
        let directory = tempdir().unwrap();
        let store = RemoteExportStateStore::new(
            directory.path().join("state"),
            source(1),
            RedactionProfile::Redacted,
        );
        let start = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).single().unwrap();
        let mut export = store.try_begin(start).unwrap();
        let initial = export.status().unwrap().cursor;
        export.append_changes(start, &[change("day-0", 0)]).unwrap();
        for day in 1_i64..=35 {
            export
                .append_changes(start + Duration::days(day), &[change(day, day as u8)])
                .unwrap();
        }
        assert_eq!(export.status().unwrap().retention_floor, 0);

        let report = export
            .append_changes(start + Duration::days(36), &[change(36, 36)])
            .unwrap();
        assert_eq!(report.pruned_by_age, 1);
        assert_eq!(report.retention_floor, 1);
        assert!(matches!(
            export.read_page(initial, 64, 64 * 1024).unwrap(),
            RemoteDeltaPageRead::CursorExpired(RemoteDeltaCursorExpired {
                reason: RemoteDeltaCursorExpiryReason::Retention,
                retention_floor: 1,
                ..
            })
        ));
    }

    #[test]
    fn source_rotation_and_redaction_profiles_are_physically_and_logically_isolated() {
        let directory = tempdir().unwrap();
        let root = directory.path().join("state");
        let redacted = RemoteExportStateStore::new(&root, source(1), RedactionProfile::Redacted);
        let redacted_cursor = {
            let mut export = redacted.try_begin(at(1, 0)).unwrap();
            export
                .append_changes(at(1, 1), &[change("redacted", 1)])
                .unwrap()
                .cursor
        };
        let preview =
            RemoteExportStateStore::new(&root, source(1), RedactionProfile::PreviewEnabled);
        let preview_export = preview.try_begin(at(1, 2)).unwrap();
        let preview_status = preview_export.status().unwrap();
        assert_eq!(preview_status.cursor.sequence, 0);
        assert_ne!(
            preview.namespace_directory(),
            redacted.namespace_directory()
        );
        assert!(matches!(
            preview_export.read_page(redacted_cursor, 10, 4096).unwrap(),
            RemoteDeltaPageRead::CursorExpired(RemoteDeltaCursorExpired {
                reason: RemoteDeltaCursorExpiryReason::GenerationMismatch,
                ..
            })
        ));
        drop(preview_export);

        let rotated = RemoteExportStateStore::new(&root, source(2), RedactionProfile::Redacted);
        assert_ne!(
            rotated.namespace_directory(),
            redacted.namespace_directory()
        );
        assert_eq!(
            rotated
                .try_begin(at(1, 3))
                .unwrap()
                .status()
                .unwrap()
                .cursor
                .sequence,
            0
        );
        assert_eq!(
            redacted
                .try_begin(at(1, 4))
                .unwrap()
                .status()
                .unwrap()
                .cursor,
            redacted_cursor
        );
    }

    #[test]
    fn exporter_lock_is_nonblocking_and_source_wide_across_profiles() {
        let directory = tempdir().unwrap();
        let root = directory.path().join("state");
        let redacted = RemoteExportStateStore::new(&root, source(1), RedactionProfile::Redacted);
        let preview =
            RemoteExportStateStore::new(&root, source(1), RedactionProfile::PreviewEnabled);
        let export = redacted.try_begin(at(1, 0)).unwrap();
        let error = preview.try_begin(at(1, 0)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        drop(export);
        assert!(preview.try_begin(at(1, 0)).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn exporter_session_rejects_a_lock_inode_replaced_after_acquisition() {
        let directory = tempdir().unwrap();
        let store = RemoteExportStateStore::new(
            directory.path().join("state"),
            source(1),
            RedactionProfile::Redacted,
        );
        let export = store.try_begin(at(1, 0)).unwrap();
        let lock_path = store.source_directory().join(EXPORT_LOCK_FILE);
        let displaced = store.source_directory().join("displaced.lock");
        fs::rename(&lock_path, &displaced).unwrap();
        fs::write(&lock_path, b"").unwrap();
        set_mode(&lock_path, 0o600);
        let error = export.status().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(
            error
                .to_string()
                .contains("changed while it was being opened")
        );
    }

    #[test]
    fn corrupt_or_missing_initialized_state_fails_closed() {
        let directory = tempdir().unwrap();
        let store = RemoteExportStateStore::new(
            directory.path().join("state"),
            source(1),
            RedactionProfile::Redacted,
        );
        drop(store.try_begin(at(1, 0)).unwrap());
        let state_path = store.state_path();
        fs::write(&state_path, b"{\"formatVersion\":999}\n").unwrap();
        #[cfg(unix)]
        set_mode(&state_path, 0o600);
        let error = store.try_begin(at(1, 1)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(fs::read(&state_path).unwrap(), b"{\"formatVersion\":999}\n");

        // Restore a valid namespace, then prove its durable anchor prevents an
        // absent state file from being mistaken for first initialization.
        fs::remove_dir_all(directory.path().join("state")).unwrap();
        drop(store.try_begin(at(1, 2)).unwrap());
        fs::remove_file(store.state_path()).unwrap();
        let error = store.try_begin(at(1, 3)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(!store.state_path().exists());
    }

    #[cfg(unix)]
    #[test]
    fn private_modes_and_symlinked_or_permissive_paths_fail_closed() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let directory = tempdir().unwrap();
        let public_root = directory.path().join("public-state");
        fs::create_dir(&public_root).unwrap();
        set_mode(&public_root, 0o755);
        let public_store =
            RemoteExportStateStore::new(&public_root, source(1), RedactionProfile::Redacted);
        assert_eq!(
            public_store.try_begin(at(1, 0)).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );

        let private_root = directory.path().join("private-state");
        let store =
            RemoteExportStateStore::new(&private_root, source(2), RedactionProfile::Redacted);
        drop(store.try_begin(at(1, 0)).unwrap());
        for path in [
            private_root.clone(),
            store.export_root(),
            store.node_directory(),
            store.source_directory(),
            store.namespace_directory(),
        ] {
            assert_eq!(
                fs::symlink_metadata(path).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        for path in [
            store.source_directory().join(EXPORT_LOCK_FILE),
            store.state_path(),
            store.anchor_path(),
        ] {
            assert_eq!(
                fs::symlink_metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }

        set_mode(&store.state_path(), 0o644);
        assert_eq!(
            store.try_begin(at(1, 1)).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        set_mode(&store.state_path(), 0o600);

        let profile = store.namespace_directory();
        let moved = private_root.join("moved-profile");
        fs::rename(&profile, &moved).unwrap();
        symlink(&moved, &profile).unwrap();
        let error = store.try_begin(at(1, 2)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(moved.join(DELTA_STATE_FILE).exists());
    }

    #[test]
    fn page_and_append_inputs_are_bounded_before_mutation() {
        let directory = tempdir().unwrap();
        let store = RemoteExportStateStore::new(
            directory.path().join("state"),
            source(1),
            RedactionProfile::Redacted,
        );
        let mut export = store.try_begin(at(1, 0)).unwrap();
        let cursor = export.status().unwrap().cursor;
        assert_eq!(
            export.read_page(cursor, 0, 1).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            export
                .read_page(cursor, MAX_PAGE_ENTRIES + 1, 1)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        let oversized =
            RemoteExportChange::new("large", vec![0; MAX_CHANGE_PAYLOAD_BYTES + 1]).unwrap_err();
        assert_eq!(oversized.kind(), io::ErrorKind::InvalidData);
        let batch = (0..=MAX_APPEND_BATCH_ENTRIES)
            .map(|index| change(index, 1))
            .collect::<Vec<_>>();
        assert_eq!(
            export.append_changes(at(1, 1), &batch).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(export.status().unwrap().cursor, cursor);
        assert_eq!(MAX_JOURNAL_STATE_BYTES, 128 * 1024 * 1024);
        assert_eq!(JOURNAL_RETENTION_DAYS, 35);
    }

    #[test]
    fn reconcile_is_idempotent_updates_partial_sets_and_preserves_aba_transitions() {
        let directory = tempdir().unwrap();
        let store = RemoteExportStateStore::new(
            directory.path().join("state"),
            source(1),
            RedactionProfile::Redacted,
        );
        let a_v1 = desired("bucket-a", 1);
        let a_v2 = desired("bucket-a", 2);
        let b_v1 = desired("bucket-b", 1);
        let mut export = store.try_begin(at(1, 0)).unwrap();
        let initial = export.status().unwrap().cursor;

        let first = export
            .reconcile_materialized_records(
                at(1, 1),
                &[a_v1.clone(), b_v1.clone()],
                RemoteExportReconcileMode::Authoritative,
            )
            .unwrap();
        assert_eq!(first.upserts_appended, 2);
        assert_eq!(first.tombstones_appended, 0);
        assert_eq!(first.current_records, 2);

        let unchanged = export
            .reconcile_materialized_records(
                at(1, 2),
                &[a_v1.clone(), b_v1.clone()],
                RemoteExportReconcileMode::Authoritative,
            )
            .unwrap();
        assert_eq!(unchanged.cursor, first.cursor);
        assert_eq!(unchanged.upserts_appended, 0);
        assert_eq!(unchanged.tombstones_appended, 0);

        // A partial scan updates A but cannot infer that omitted B vanished.
        let partial = export
            .reconcile_materialized_records(
                at(1, 3),
                std::slice::from_ref(&a_v2),
                RemoteExportReconcileMode::UpsertOnly,
            )
            .unwrap();
        assert_eq!(partial.upserts_appended, 1);
        assert_eq!(partial.tombstones_appended, 0);
        assert_eq!(partial.current_records, 2);

        let deleted = export
            .reconcile_materialized_records(
                at(1, 4),
                std::slice::from_ref(&a_v2),
                RemoteExportReconcileMode::Authoritative,
            )
            .unwrap();
        assert_eq!(deleted.tombstones_appended, 1);
        assert_eq!(deleted.current_records, 1);

        // Reappearance and a second deletion are real transitions even though
        // their deterministic content IDs and payload bytes repeat exactly.
        let resurrected = export
            .reconcile_materialized_records(
                at(1, 5),
                &[a_v2.clone(), b_v1.clone()],
                RemoteExportReconcileMode::Authoritative,
            )
            .unwrap();
        assert_eq!(resurrected.upserts_appended, 1);
        let deleted_again = export
            .reconcile_materialized_records(
                at(1, 6),
                std::slice::from_ref(&a_v2),
                RemoteExportReconcileMode::Authoritative,
            )
            .unwrap();
        assert_eq!(deleted_again.tombstones_appended, 1);

        let journal = page(export.read_page(initial, 32, 64 * 1024).unwrap());
        assert_eq!(
            entry_ids(&journal),
            vec![
                "upsert-bucket-a-v1",
                "upsert-bucket-b-v1",
                "upsert-bucket-a-v2",
                "tombstone-bucket-b",
                "upsert-bucket-b-v1",
                "tombstone-bucket-b",
            ]
        );
        drop(export);

        let mut reopened = store.try_begin(at(1, 7)).unwrap();
        assert_eq!(reopened.status().unwrap().materialized_records, 1);
        let restart_noop = reopened
            .reconcile_materialized_records(
                at(1, 7),
                std::slice::from_ref(&a_v2),
                RemoteExportReconcileMode::Authoritative,
            )
            .unwrap();
        assert_eq!(restart_noop.cursor, deleted_again.cursor);
        assert_eq!(restart_noop.upserts_appended, 0);
        assert_eq!(restart_noop.tombstones_appended, 0);
    }

    #[test]
    fn unchanged_reconcile_rate_limits_retention_checkpoint_rewrites() {
        let directory = tempdir().unwrap();
        let store = RemoteExportStateStore::new(
            directory.path().join("state"),
            source(1),
            RedactionProfile::Redacted,
        );
        let record = desired("bucket-a", 1);
        let mut export = store.try_begin(at(1, 0)).unwrap();
        export
            .reconcile_materialized_records(
                at(1, 0),
                std::slice::from_ref(&record),
                RemoteExportReconcileMode::Authoritative,
            )
            .unwrap();
        let before = fs::read(store.state_path()).unwrap();

        let unchanged = export
            .reconcile_materialized_records(
                at(1, 1),
                std::slice::from_ref(&record),
                RemoteExportReconcileMode::Authoritative,
            )
            .unwrap();
        assert_eq!(unchanged.upserts_appended, 0);
        assert_eq!(unchanged.tombstones_appended, 0);
        assert_eq!(fs::read(store.state_path()).unwrap(), before);

        let checkpoint = export
            .reconcile_materialized_records(
                at(1, 0) + Duration::hours(RECONCILE_MAINTENANCE_INTERVAL_HOURS),
                std::slice::from_ref(&record),
                RemoteExportReconcileMode::Authoritative,
            )
            .unwrap();
        assert_eq!(checkpoint.upserts_appended, 0);
        assert_eq!(checkpoint.tombstones_appended, 0);
        assert_ne!(fs::read(store.state_path()).unwrap(), before);
    }

    #[test]
    fn persisted_ttl_expires_partial_omissions_and_prevents_republication() {
        let directory = tempdir().unwrap();
        let store = RemoteExportStateStore::new(
            directory.path().join("state"),
            source(1),
            RedactionProfile::Redacted,
        );
        let record = desired_until("bucket-a", 1, at(3, 0));
        let mut export = store.try_begin(at(1, 0)).unwrap();
        let initial = export.status().unwrap().cursor;
        export
            .reconcile_materialized_records(
                at(1, 0),
                std::slice::from_ref(&record),
                RemoteExportReconcileMode::UpsertOnly,
            )
            .unwrap();

        let omitted = export
            .reconcile_materialized_records(at(2, 0), &[], RemoteExportReconcileMode::UpsertOnly)
            .unwrap();
        assert_eq!(omitted.current_records, 1);
        assert_eq!(omitted.tombstones_appended, 0);

        let expired = export
            .reconcile_materialized_records(
                at(3, 0),
                std::slice::from_ref(&record),
                RemoteExportReconcileMode::UpsertOnly,
            )
            .unwrap();
        assert_eq!(expired.current_records, 0);
        assert_eq!(expired.tombstones_appended, 1);
        assert_eq!(expired.expired_records, 1);
        let page = page(export.read_page(initial, 16, 64 * 1024).unwrap());
        assert_eq!(
            entry_ids(&page),
            vec!["upsert-bucket-a-v1", "tombstone-bucket-a"]
        );

        let stale_input = export
            .reconcile_materialized_records(
                at(4, 0),
                std::slice::from_ref(&record),
                RemoteExportReconcileMode::UpsertOnly,
            )
            .unwrap();
        assert_eq!(stale_input.current_records, 0);
        assert_eq!(stale_input.cursor, expired.cursor);
        drop(export);
        assert_eq!(
            store
                .try_begin(at(4, 1))
                .unwrap()
                .status()
                .unwrap()
                .materialized_records,
            0
        );
    }

    #[test]
    fn direct_append_compaction_expires_materialized_records_atomically() {
        let directory = tempdir().unwrap();
        let store = RemoteExportStateStore::new(
            directory.path().join("state"),
            source(1),
            RedactionProfile::Redacted,
        );
        let record = desired_until("bucket-a", 1, at(2, 0));
        let mut export = store.try_begin(at(1, 0)).unwrap();
        let initial = export.status().unwrap().cursor;
        export
            .reconcile_materialized_records(
                at(1, 0),
                std::slice::from_ref(&record),
                RemoteExportReconcileMode::UpsertOnly,
            )
            .unwrap();
        let report = export
            .append_changes(at(2, 0), &[change("unrelated", 7)])
            .unwrap();
        assert_eq!(report.appended, 2);
        assert_eq!(report.expired_records, 1);
        assert_eq!(export.status().unwrap().materialized_records, 0);
        assert_eq!(
            entry_ids(&page(export.read_page(initial, 16, 64 * 1024).unwrap())),
            vec![
                "upsert-bucket-a-v1",
                "change-unrelated",
                "tombstone-bucket-a"
            ]
        );
    }

    #[test]
    fn suspicious_forward_clock_defers_ttl_expiry_until_confirmed() {
        let directory = tempdir().unwrap();
        let store = RemoteExportStateStore::new(
            directory.path().join("state"),
            source(1),
            RedactionProfile::Redacted,
        );
        let jan_1 = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).single().unwrap();
        let expires = jan_1 + Duration::days(2);
        let record = desired_until("bucket-a", 1, expires);
        let mut export = store.try_begin(jan_1).unwrap();
        export
            .reconcile_materialized_records(
                jan_1,
                std::slice::from_ref(&record),
                RemoteExportReconcileMode::UpsertOnly,
            )
            .unwrap();

        let jump = jan_1 + Duration::days(40);
        for (offset, expected_deferred) in [(0, true), (1, true), (2, false)] {
            let report = export
                .reconcile_materialized_records(
                    jump + Duration::days(offset),
                    &[],
                    RemoteExportReconcileMode::UpsertOnly,
                )
                .unwrap();
            assert_eq!(report.retention_deferred, expected_deferred);
            assert_eq!(report.current_records, usize::from(expected_deferred));
            assert_eq!(report.expired_records, usize::from(!expected_deferred));
        }
    }

    #[test]
    fn unchanged_reconcile_clears_a_future_confirmation_after_clock_rollback() {
        let directory = tempdir().unwrap();
        let store = RemoteExportStateStore::new(
            directory.path().join("state"),
            source(1),
            RedactionProfile::Redacted,
        );
        let jan_1 = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).single().unwrap();
        let record = desired_until("bucket-a", 1, jan_1 + Duration::days(100));
        let mut export = store.try_begin(jan_1).unwrap();
        export
            .reconcile_materialized_records(
                jan_1,
                std::slice::from_ref(&record),
                RemoteExportReconcileMode::Authoritative,
            )
            .unwrap();

        let jumped = export
            .reconcile_materialized_records(
                jan_1 + Duration::days(40),
                std::slice::from_ref(&record),
                RemoteExportReconcileMode::Authoritative,
            )
            .unwrap();
        assert!(jumped.retention_deferred);

        let corrected = export
            .reconcile_materialized_records(
                jan_1 + Duration::hours(12),
                std::slice::from_ref(&record),
                RemoteExportReconcileMode::Authoritative,
            )
            .unwrap();
        assert!(!corrected.retention_deferred);
        let persisted: StoredExportState =
            serde_json::from_slice(&fs::read(store.state_path()).unwrap()).unwrap();
        assert_eq!(
            persisted.retention_clock,
            RetentionClock::current(jan_1 + Duration::hours(12))
        );
    }

    #[test]
    fn existing_logical_key_cannot_extend_or_shorten_its_persisted_ttl() {
        let directory = tempdir().unwrap();
        let store = RemoteExportStateStore::new(
            directory.path().join("state"),
            source(1),
            RedactionProfile::Redacted,
        );
        let record = desired_until("bucket-a", 1, at(10, 0));
        let changed_ttl = desired_until("bucket-a", 2, at(11, 0));
        let mut export = store.try_begin(at(1, 0)).unwrap();
        export
            .reconcile_materialized_records(
                at(1, 0),
                std::slice::from_ref(&record),
                RemoteExportReconcileMode::UpsertOnly,
            )
            .unwrap();
        let before = export.status().unwrap();
        let error = export
            .reconcile_materialized_records(
                at(2, 0),
                &[changed_ttl],
                RemoteExportReconcileMode::UpsertOnly,
            )
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(export.status().unwrap(), before);
    }

    #[test]
    fn retention_reappends_unchanged_current_upsert_for_floor_bootstrap() {
        let directory = tempdir().unwrap();
        let store = RemoteExportStateStore::new(
            directory.path().join("state"),
            source(1),
            RedactionProfile::Redacted,
        )
        .with_limits(RemoteExportLimits {
            retention_days: 0,
            ..RemoteExportLimits::default()
        });
        let record = desired("bucket-a", 1);
        let mut export = store.try_begin(at(1, 0)).unwrap();
        export
            .reconcile_materialized_records(
                at(1, 0),
                std::slice::from_ref(&record),
                RemoteExportReconcileMode::Authoritative,
            )
            .unwrap();
        let refreshed = export
            .reconcile_materialized_records(
                at(1, 1),
                std::slice::from_ref(&record),
                RemoteExportReconcileMode::Authoritative,
            )
            .unwrap();
        assert_eq!(refreshed.pruned_by_age, 1);
        assert_eq!(refreshed.upserts_appended, 1);
        assert_eq!(refreshed.bootstrap_upserts_appended, 1);
        assert_eq!(refreshed.retention_floor, 1);

        let bootstrap = page(
            export
                .read_page(
                    DeltaCursor {
                        generation: refreshed.cursor.generation,
                        sequence: refreshed.retention_floor,
                    },
                    16,
                    64 * 1024,
                )
                .unwrap(),
        );
        assert_eq!(entry_ids(&bootstrap), vec!["upsert-bucket-a-v1"]);
        assert_eq!(bootstrap.entries[0].sequence(), 2);
    }

    #[test]
    fn capacity_compaction_refreshes_all_current_upserts_after_the_floor() {
        let directory = tempdir().unwrap();
        let store = RemoteExportStateStore::new(
            directory.path().join("state"),
            source(1),
            RedactionProfile::Redacted,
        )
        .with_limits(RemoteExportLimits {
            maximum_entries: 2,
            ..RemoteExportLimits::default()
        });
        let desired = [desired("bucket-a", 1), desired("bucket-b", 1)];
        let mut export = store.try_begin(at(1, 0)).unwrap();
        export
            .reconcile_materialized_records(
                at(1, 0),
                &desired,
                RemoteExportReconcileMode::Authoritative,
            )
            .unwrap();
        let report = export
            .append_changes(at(1, 1), &[change("unrelated", 9)])
            .unwrap();
        assert_eq!(report.appended, 1);
        assert_eq!(report.materialized_refreshes, 2);
        assert_eq!(report.retention_floor, 3);

        let bootstrap = page(
            export
                .read_page(
                    DeltaCursor {
                        generation: report.cursor.generation,
                        sequence: report.retention_floor,
                    },
                    16,
                    64 * 1024,
                )
                .unwrap(),
        );
        assert_eq!(
            entry_ids(&bootstrap),
            vec!["upsert-bucket-a-v1", "upsert-bucket-b-v1"]
        );
        assert_eq!(
            bootstrap
                .entries
                .iter()
                .map(RemoteExportJournalEntry::sequence)
                .collect::<Vec<_>>(),
            vec![4, 5]
        );
    }

    #[test]
    fn reconcile_rejects_noncanonical_or_conflicting_input_without_mutation() {
        let directory = tempdir().unwrap();
        let store = RemoteExportStateStore::new(
            directory.path().join("state"),
            source(1),
            RedactionProfile::Redacted,
        );
        let a = desired("bucket-a", 1);
        let b = desired("bucket-b", 1);
        let mut export = store.try_begin(at(1, 0)).unwrap();
        export
            .reconcile_materialized_records(
                at(1, 0),
                std::slice::from_ref(&a),
                RemoteExportReconcileMode::Authoritative,
            )
            .unwrap();
        let before = export.status().unwrap();

        let error = export
            .reconcile_materialized_records(
                at(1, 1),
                &[b.clone(), a.clone()],
                RemoteExportReconcileMode::Authoritative,
            )
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(export.status().unwrap(), before);

        let shared = RemoteExportChange::new("shared-upsert", vec![7; 8]).unwrap();
        let first = RemoteExportDesiredRecord::new(
            "bucket-a",
            at(31, 0),
            shared.clone(),
            change("tomb-a", 1),
        )
        .unwrap();
        let second =
            RemoteExportDesiredRecord::new("bucket-b", at(31, 0), shared, change("tomb-b", 2))
                .unwrap();
        let error = export
            .reconcile_materialized_records(
                at(1, 1),
                &[first, second],
                RemoteExportReconcileMode::Authoritative,
            )
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(export.status().unwrap(), before);
        drop(export);
        assert_eq!(store.try_begin(at(1, 2)).unwrap().status().unwrap(), before);
    }

    #[test]
    fn failed_or_interrupted_reconcile_keeps_the_previous_atomic_state() {
        let directory = tempdir().unwrap();
        let root = directory.path().join("state");
        let base = RemoteExportStateStore::new(&root, source(1), RedactionProfile::Redacted);
        let a = desired("bucket-a", 1);
        let before = {
            let mut export = base.try_begin(at(1, 0)).unwrap();
            export
                .reconcile_materialized_records(
                    at(1, 0),
                    std::slice::from_ref(&a),
                    RemoteExportReconcileMode::Authoritative,
                )
                .unwrap();
            export.status().unwrap()
        };
        let current_size = encoded_state_size(&base.read_state().unwrap()).unwrap();
        let constrained = base.clone().with_limits(RemoteExportLimits {
            maximum_state_bytes: current_size,
            ..RemoteExportLimits::default()
        });
        let mut export = constrained.try_begin(at(1, 1)).unwrap();
        let b = desired("bucket-b", 1);
        let error = export
            .reconcile_materialized_records(
                at(1, 1),
                &[a.clone(), b],
                RemoteExportReconcileMode::Authoritative,
            )
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(export.status().unwrap(), before);
        drop(export);

        // A crash before atomic replacement can leave a private temporary
        // file, but restart must continue reading only the published state.
        let interrupted = base
            .namespace_directory()
            .join(format!(".{DELTA_STATE_FILE}.interrupted.tmp"));
        fs::write(&interrupted, b"{\"partial\":true").unwrap();
        #[cfg(unix)]
        set_mode(&interrupted, 0o600);
        let mut reopened = base.try_begin(at(1, 2)).unwrap();
        assert_eq!(reopened.status().unwrap(), before);
        let no_op = reopened
            .reconcile_materialized_records(
                at(1, 2),
                std::slice::from_ref(&a),
                RemoteExportReconcileMode::Authoritative,
            )
            .unwrap();
        assert_eq!(no_op.cursor, before.cursor);
    }

    #[test]
    fn reconcile_sequence_overflow_fails_before_appending() {
        let store = RemoteExportStateStore::new(
            PathBuf::from("unused"),
            source(1),
            RedactionProfile::Redacted,
        );
        let mut state =
            StoredExportState::new(source(1), RedactionProfile::Redacted, at(1, 0)).unwrap();
        state.current_sequence = u64::MAX;
        state.retention_floor = u64::MAX;
        let error = reconcile_and_compact(
            &mut state,
            at(1, 1),
            &[desired("bucket-a", 1)],
            RemoteExportReconcileMode::Authoritative,
            &store,
        )
        .unwrap_err();
        assert!(error.to_string().contains("sequence overflowed"));
        assert_eq!(state.current_sequence, u64::MAX);
        assert!(state.entries.is_empty());
        assert!(state.materialized_records.is_empty());
    }

    #[cfg(unix)]
    fn set_mode(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;

        let mut permissions = fs::symlink_metadata(path).unwrap().permissions();
        permissions.set_mode(mode);
        fs::set_permissions(path, permissions).unwrap();
    }
}
