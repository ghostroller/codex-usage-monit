//! Explicit, currently unwired bridge from local v1 history into source-aware
//! v2 history.
//!
//! The bridge locks v1 while copying and committing its marker, but the marker
//! does not teach old recorder processes to stop writing v1. Runtime cutover
//! must first flush and quiesce every v1 writer, invoke this importer, and then
//! switch ownership to v2 without restarting an old writer. Until that
//! orchestration exists, this module must remain opt-in and unwired.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::atomic_file::replace_file;
use crate::history::{HISTORY_RETENTION_DAYS, HistoryData, HistoryStore};
use crate::history_ownership::{
    HistoryOwnershipManifest, HistoryOwnershipState, HistoryOwnershipStore, HistoryWriterLease,
    OwnershipManifestStatus,
};
use crate::source_history::{
    AccountHistoryData, HistoryProfileId, RedactionProfile, SourceBucketChange, SourceBucketRecord,
    SourceHistoryData, SourceHistoryStore, SourceHistoryWriter, SourceKind, SourceMetadata,
    SourceWeeklyChange, SourceWeeklyRecord,
};
use crate::source_identity::{NodeId, SourceIdentity};
#[cfg(windows)]
use crate::source_identity::{validate_windows_private_directory, validate_windows_private_file};

pub const LOCAL_V1_MIGRATION_STATE_VERSION: u32 = 4;

const IMPORTS_DIRECTORY: &str = "imports";
const LEGACY_HISTORY_DIRECTORY: &str = "history-v1";
const MIGRATION_LOCK_FILE: &str = "local-v1.lock";
const MIGRATION_STATE_PREFIX: &str = "local-v1-";
const MAX_STATE_FILE_BYTES: u64 = 16 * 1024;
const MAX_IMPORTED_QUOTA_POINTS: usize = 250_000;
const MAX_IMPORTED_BUCKETS: usize = (HISTORY_RETENTION_DAYS as usize + 2) * 24 * 60 / 15;
const MAX_IMPORTED_WEEKLY_POINTS: usize = (HISTORY_RETENTION_DAYS as usize + 2) * 24 * 2;
const TEMP_FILE_ATTEMPTS: usize = 128;
const FROZEN_V1_DIGEST_DOMAIN: &[u8] = b"codex-usage-monit/local-v1-snapshot/v1\0";
static TEMP_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug)]
pub struct LocalV1MigrationOptions<'a> {
    pub source_identity: &'a SourceIdentity,
    pub redaction_profile: RedactionProfile,
    pub source_label: &'a str,
    /// Cutover epoch from the current `Migrating` ownership manifest.
    pub expected_ownership_epoch: u64,
    pub window_starts_at: DateTime<Utc>,
    pub completed_at: DateTime<Utc>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LocalV1MigrationOutcome {
    Imported,
    AlreadyComplete,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalV1MigrationReport {
    pub outcome: LocalV1MigrationOutcome,
    pub attempt: u64,
    pub quota_points: u64,
    pub buckets: u64,
    pub weekly_local_points: u64,
    pub account_shards_written: usize,
    pub source_shards_written: usize,
    pub source_weekly_shards_written: usize,
    /// Always true in this unwired phase: a complete marker does not prevent
    /// an old recorder from appending a new v1-only tail after this call.
    pub v1_writer_cutover_required: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MigratedLocalHistory {
    pub migration_window_starts_at: DateTime<Utc>,
    pub account: AccountHistoryData,
    pub source: SourceHistoryData,
}

/// Durable marker state observed while the ownership manifest is still
/// `Migrating`. A missing marker is safe to initialize only when the target
/// source namespace is also absent; ambiguous combinations fail closed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LocalV1MigrationRecoveryStatus {
    Missing,
    Running(LocalV1MigrationCheckpoint),
    Complete(LocalV1MigrationCheckpoint),
}

/// Validated, epoch-bound facts from a running or complete migration marker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalV1MigrationCheckpoint {
    ownership_epoch: u64,
    attempt: u64,
    window_starts_at: DateTime<Utc>,
    completed_at: Option<DateTime<Utc>>,
    quota_points: Option<u64>,
    buckets: Option<u64>,
    weekly_local_points: Option<u64>,
}

impl LocalV1MigrationCheckpoint {
    pub fn ownership_epoch(&self) -> u64 {
        self.ownership_epoch
    }

    pub fn attempt(&self) -> u64 {
        self.attempt
    }

    pub fn window_starts_at(&self) -> DateTime<Utc> {
        self.window_starts_at
    }

    pub fn completed_at(&self) -> Option<DateTime<Utc>> {
        self.completed_at
    }

    pub fn quota_points(&self) -> Option<u64> {
        self.quota_points
    }

    pub fn buckets(&self) -> Option<u64> {
        self.buckets
    }

    pub fn weekly_local_points(&self) -> Option<u64> {
        self.weekly_local_points
    }
}

/// Opaque proof that a complete marker and a freshly frozen v1 snapshot were
/// revalidated against v2 while the caller held the current writer lease.
/// Future orchestration must still keep that lease and compare-and-transition
/// the same `Migrating` manifest immediately after receiving this value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalV1MigrationActivationEvidence {
    ownership_epoch: u64,
    attempt: u64,
    completed_at: DateTime<Utc>,
}

impl LocalV1MigrationActivationEvidence {
    pub fn ownership_epoch(&self) -> u64 {
        self.ownership_epoch
    }

    pub fn attempt(&self) -> u64 {
        self.attempt
    }

    pub fn completed_at(&self) -> DateTime<Utc> {
        self.completed_at
    }
}

/// Successful, atomic activation result. The ownership manifest reached
/// `V2Active` in the same writer-lease and frozen-v1 critical section that
/// produced the verification evidence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalV1MigrationActivation {
    evidence: LocalV1MigrationActivationEvidence,
    ownership: HistoryOwnershipManifest,
}

struct CompleteMigrationVerification<'a, 'options> {
    target: &'a SourceHistoryStore,
    ownership: &'a HistoryOwnershipStore,
    writer_lease: &'a HistoryWriterLease,
    expected_ownership: &'a HistoryOwnershipManifest,
    options: &'a LocalV1MigrationOptions<'options>,
    state: &'a MigrationState,
}

enum CompleteMigrationInspection<T> {
    Verified(T),
    V1SnapshotChanged,
}

impl LocalV1MigrationActivation {
    pub fn evidence(&self) -> &LocalV1MigrationActivationEvidence {
        &self.evidence
    }

    pub fn ownership(&self) -> &HistoryOwnershipManifest {
        &self.ownership
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum MigrationStatus {
    Running,
    Complete,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct MigrationState {
    format_version: u32,
    profile_id: HistoryProfileId,
    v1_namespace: String,
    source_id: NodeId,
    source_generation: u64,
    redaction_profile: RedactionProfile,
    expected_ownership_epoch: u64,
    window_starts_at: DateTime<Utc>,
    attempt: u64,
    status: MigrationStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    completed_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    quota_points: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    buckets: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    weekly_local_points: Option<u64>,
    /// SHA-256 over the exact v1 vectors frozen for the completed import.
    /// Running attempts have no digest; complete markers must always carry
    /// one so recovery can distinguish a later v1 tail from v2 corruption.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    frozen_v1_digest: Option<String>,
}

impl MigrationState {
    fn first_attempt(
        profile_id: HistoryProfileId,
        v1_namespace: String,
        options: &LocalV1MigrationOptions<'_>,
    ) -> Self {
        Self {
            format_version: LOCAL_V1_MIGRATION_STATE_VERSION,
            profile_id,
            v1_namespace,
            source_id: options.source_identity.node_id().clone(),
            source_generation: options.source_identity.generation(),
            redaction_profile: options.redaction_profile,
            expected_ownership_epoch: options.expected_ownership_epoch,
            window_starts_at: options.window_starts_at,
            attempt: 1,
            status: MigrationStatus::Running,
            completed_at: None,
            quota_points: None,
            buckets: None,
            weekly_local_points: None,
            frozen_v1_digest: None,
        }
    }

    fn validate(&self) -> io::Result<()> {
        if self.format_version != LOCAL_V1_MIGRATION_STATE_VERSION {
            return Err(invalid_data(format!(
                "unsupported local v1 migration state version {}; expected {}",
                self.format_version, LOCAL_V1_MIGRATION_STATE_VERSION
            )));
        }
        if self.v1_namespace.is_empty()
            || self.attempt == 0
            || self.source_generation == 0
            || self.expected_ownership_epoch <= 1
        {
            return Err(invalid_data("local v1 migration state is invalid"));
        }
        match self.status {
            MigrationStatus::Running
                if self.completed_at.is_none()
                    && self.quota_points.is_none()
                    && self.buckets.is_none()
                    && self.weekly_local_points.is_none()
                    && self.frozen_v1_digest.is_none() =>
            {
                Ok(())
            }
            MigrationStatus::Complete
                if self.completed_at.is_some()
                    && self.completed_at >= Some(self.window_starts_at)
                    && self.quota_points.is_some()
                    && self.buckets.is_some()
                    && self.weekly_local_points.is_some()
                    && self
                        .frozen_v1_digest
                        .as_deref()
                        .is_some_and(valid_frozen_v1_digest) =>
            {
                Ok(())
            }
            _ => Err(invalid_data(
                "local v1 migration state has inconsistent status fields",
            )),
        }
    }

    fn validate_binding(
        &self,
        store: &SourceHistoryStore,
        options: &LocalV1MigrationOptions<'_>,
        v1_namespace: &str,
    ) -> io::Result<()> {
        self.validate()?;
        if &self.profile_id != store.profile_id()
            || self.v1_namespace != v1_namespace
            || &self.source_id != options.source_identity.node_id()
            || self.source_generation != options.source_identity.generation()
            || self.redaction_profile != options.redaction_profile
            || self.expected_ownership_epoch != options.expected_ownership_epoch
            || self.window_starts_at != options.window_starts_at
        {
            return Err(invalid_data(
                "local v1 migration state does not match the requested source and namespace",
            ));
        }
        Ok(())
    }

    fn validate_query_binding(
        &self,
        store: &SourceHistoryStore,
        identity: &SourceIdentity,
        redaction_profile: RedactionProfile,
        expected_ownership_epoch: u64,
    ) -> io::Result<()> {
        self.validate()?;
        if &self.profile_id != store.profile_id()
            || &self.source_id != identity.node_id()
            || self.source_generation != identity.generation()
            || self.redaction_profile != redaction_profile
            || self.expected_ownership_epoch != expected_ownership_epoch
        {
            return Err(invalid_data(
                "local v1 migration state does not match the requested source and namespace",
            ));
        }
        Ok(())
    }

    fn checkpoint(&self) -> LocalV1MigrationCheckpoint {
        LocalV1MigrationCheckpoint {
            ownership_epoch: self.expected_ownership_epoch,
            attempt: self.attempt,
            window_starts_at: self.window_starts_at,
            completed_at: self.completed_at,
            quota_points: self.quota_points,
            buckets: self.buckets,
            weekly_local_points: self.weekly_local_points,
        }
    }

    fn validate_complete_snapshot_counts(&self, snapshot: &HistoryData) -> io::Result<()> {
        self.validate()?;
        if self.status != MigrationStatus::Complete {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "local v1 migration is not complete",
            ));
        }
        let counts = (
            usize::try_from(self.quota_points.unwrap_or(0)),
            usize::try_from(self.buckets.unwrap_or(0)),
            usize::try_from(self.weekly_local_points.unwrap_or(0)),
        );
        match counts {
            (Ok(quota_points), Ok(buckets), Ok(weekly_local_points))
                if quota_points == snapshot.quota_points.len()
                    && buckets == snapshot.half_hour_buckets.len()
                    && weekly_local_points == snapshot.weekly_local_points.len() =>
            {
                Ok(())
            }
            _ => Err(invalid_data(
                "completed local v1 migration marker does not match the frozen v1 snapshot",
            )),
        }
    }

    fn start_retry(&mut self) -> io::Result<()> {
        self.attempt = self
            .attempt
            .checked_add(1)
            .ok_or_else(|| invalid_data("local v1 migration attempt counter overflowed"))?;
        self.completed_at = None;
        self.quota_points = None;
        self.buckets = None;
        self.weekly_local_points = None;
        self.frozen_v1_digest = None;
        self.status = MigrationStatus::Running;
        Ok(())
    }

    fn complete(
        &mut self,
        completed_at: DateTime<Utc>,
        quota_points: usize,
        buckets: usize,
        weekly_local_points: usize,
        frozen_v1_digest: String,
    ) -> io::Result<()> {
        if !valid_frozen_v1_digest(&frozen_v1_digest) {
            return Err(invalid_data(
                "local v1 migration snapshot digest is invalid",
            ));
        }
        self.completed_at = Some(completed_at);
        self.quota_points = Some(u64::try_from(quota_points).map_err(|_| {
            invalid_data("local v1 migration quota-point count does not fit in u64")
        })?);
        self.buckets =
            Some(u64::try_from(buckets).map_err(|_| {
                invalid_data("local v1 migration bucket count does not fit in u64")
            })?);
        self.weekly_local_points = Some(u64::try_from(weekly_local_points).map_err(|_| {
            invalid_data("local v1 migration weekly-point count does not fit in u64")
        })?);
        self.frozen_v1_digest = Some(frozen_v1_digest);
        self.status = MigrationStatus::Complete;
        Ok(())
    }
}

/// Imports one frozen v1 namespace into the source-aware v2 layout.
///
/// The migration state is published as `running` before any v2 write. A retry
/// increments `attempt`, which is also the source-bucket revision, so a crash
/// after partial publication can be repaired deterministically. Live records
/// from earlier attempts that disappeared from the retained v1 snapshot are
/// tombstoned at the new attempt revision before exact verification. The v1
/// lock is held until the complete marker is durable. Runtime integration must
/// stop scheduling future v1 writes before invoking this cutover operation.
pub fn migrate_local_v1_history(
    legacy: &mut HistoryStore,
    target: &SourceHistoryStore,
    ownership: &HistoryOwnershipStore,
    writer_lease: &HistoryWriterLease,
    expected_ownership: &HistoryOwnershipManifest,
    options: &LocalV1MigrationOptions<'_>,
) -> io::Result<LocalV1MigrationReport> {
    validate_current_migrating_ownership(
        ownership,
        writer_lease,
        expected_ownership,
        target,
        options.redaction_profile,
        options.expected_ownership_epoch,
    )?;
    let write_authority = ownership.authorize_v2_write(writer_lease, expected_ownership)?;
    let writer = target.writer(&write_authority)?;
    let v1_namespace = validate_store_binding(legacy, target, options.redaction_profile)?;
    validate_migration_options(options)?;
    if options.window_starts_at > options.completed_at {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "local v1 migration window starts after its completion time",
        ));
    }
    // Validate the label before publishing a running marker.
    let proposed_metadata = SourceMetadata::new_with_redaction_profile(
        options.source_identity.node_id().clone(),
        SourceKind::Local,
        options.source_label,
        options.redaction_profile,
    )?;

    let imports_directory = imports_directory(target);
    target.prepare_private_directory(&imports_directory)?;
    let lock = open_lock_file(&imports_directory)?;
    lock_exclusive(&lock, &imports_directory)?;
    let state_path = migration_state_path(target, options.redaction_profile);

    let mut state = match read_optional_state(&state_path)? {
        Some(mut state) => {
            state.validate_binding(target, options, &v1_namespace)?;
            if state.status == MigrationStatus::Complete {
                match inspect_complete_state_with_legacy(
                    legacy,
                    CompleteMigrationVerification {
                        target,
                        ownership,
                        writer_lease,
                        expected_ownership,
                        options,
                        state: &state,
                    },
                    Ok,
                )? {
                    CompleteMigrationInspection::Verified(evidence) => {
                        debug_assert_eq!(
                            evidence.ownership_epoch(),
                            state.expected_ownership_epoch
                        );
                        return Ok(report_from_complete_state(
                            &state,
                            LocalV1MigrationOutcome::AlreadyComplete,
                        ));
                    }
                    CompleteMigrationInspection::V1SnapshotChanged => {}
                }
            }
            validate_existing_migration_revisions(
                target,
                options.source_identity.node_id(),
                options.redaction_profile,
                options.window_starts_at,
                state.attempt,
            )?;
            state.start_retry()?;
            state
        }
        None => {
            ensure_uninitialized_source_namespace(
                target,
                options.source_identity.node_id(),
                options.redaction_profile,
            )?;
            MigrationState::first_attempt(target.profile_id().clone(), v1_namespace, options)
        }
    };
    validate_current_migrating_ownership(
        ownership,
        writer_lease,
        expected_ownership,
        target,
        options.redaction_profile,
        state.expected_ownership_epoch,
    )?;
    write_state(&state_path, &state)?;

    let attempt = state.attempt;
    legacy.with_exclusive_persisted_snapshot_since(options.window_starts_at, |snapshot| {
        validate_snapshot(snapshot)?;
        ensure_import_bounds(snapshot)?;
        let frozen_v1_digest = frozen_v1_digest(snapshot)?;
        ensure_local_source(target, &writer, &proposed_metadata)?;

        let (records, weekly_records) = reconcile_migration_source_changes(
            target,
            options.source_identity.node_id(),
            options.redaction_profile,
            options.window_starts_at,
            attempt,
            snapshot,
        )?;
        let account_report = writer.record_account_points(&snapshot.quota_points)?;
        let source_report = writer.record_source_bucket_changes(
            options.source_identity.node_id(),
            options.redaction_profile,
            &records,
        )?;
        let weekly_report = writer.record_source_weekly_changes(
            options.source_identity.node_id(),
            options.redaction_profile,
            &weekly_records,
        )?;
        verify_account_snapshot(target, options.window_starts_at, snapshot)?;
        verify_source_snapshot(
            target,
            options.source_identity.node_id(),
            options.redaction_profile,
            options.window_starts_at,
            snapshot,
        )?;
        writer.ensure_local_observation_revision_floor(
            options.source_identity,
            options.redaction_profile,
            attempt,
        )?;

        state.complete(
            options.completed_at,
            snapshot.quota_points.len(),
            snapshot.half_hour_buckets.len(),
            snapshot.weekly_local_points.len(),
            frozen_v1_digest,
        )?;
        validate_current_migrating_ownership(
            ownership,
            writer_lease,
            expected_ownership,
            target,
            options.redaction_profile,
            state.expected_ownership_epoch,
        )?;
        write_state(&state_path, &state)?;
        validate_current_migrating_ownership(
            ownership,
            writer_lease,
            expected_ownership,
            target,
            options.redaction_profile,
            state.expected_ownership_epoch,
        )?;

        Ok(LocalV1MigrationReport {
            outcome: LocalV1MigrationOutcome::Imported,
            attempt,
            quota_points: state.quota_points.unwrap_or(0),
            buckets: state.buckets.unwrap_or(0),
            weekly_local_points: state.weekly_local_points.unwrap_or(0),
            account_shards_written: account_report.shards_written,
            source_shards_written: source_report.shards_written,
            source_weekly_shards_written: weekly_report.shards_written,
            v1_writer_cutover_required: true,
        })
    })
}

fn validate_migration_options(options: &LocalV1MigrationOptions<'_>) -> io::Result<()> {
    if options.expected_ownership_epoch <= 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "local v1 migration requires a cutover ownership epoch",
        ));
    }
    Ok(())
}

fn validate_ownership_store_binding(
    ownership: &HistoryOwnershipStore,
    target: &SourceHistoryStore,
    redaction_profile: RedactionProfile,
) -> io::Result<()> {
    if ownership.state_root() != target.state_root()
        || ownership.profile_id() != target.profile_id()
        || ownership.redaction_profile() != redaction_profile
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "history ownership namespace does not match the migration target",
        ));
    }
    Ok(())
}

fn validate_ownership_manifest_binding(
    ownership: &HistoryOwnershipStore,
    expected: &HistoryOwnershipManifest,
    required_state: HistoryOwnershipState,
    expected_epoch: u64,
) -> io::Result<()> {
    if expected.profile_id() != ownership.profile_id()
        || expected.redaction_profile() != ownership.redaction_profile()
        || expected.state() != required_state
        || expected.epoch() != expected_epoch
    {
        return Err(invalid_data(
            "history ownership manifest does not match the migration epoch and state",
        ));
    }
    match ownership.load_manifest()? {
        OwnershipManifestStatus::Initialized(current) if current == *expected => Ok(()),
        OwnershipManifestStatus::Initialized(_) | OwnershipManifestStatus::Uninitialized => Err(
            invalid_data("history ownership changed while validating the migration"),
        ),
    }
}

fn validate_current_migrating_ownership(
    ownership: &HistoryOwnershipStore,
    writer_lease: &HistoryWriterLease,
    expected: &HistoryOwnershipManifest,
    target: &SourceHistoryStore,
    redaction_profile: RedactionProfile,
    expected_epoch: u64,
) -> io::Result<()> {
    validate_ownership_store_binding(ownership, target, redaction_profile)?;
    ownership.validate_writer_lease(writer_lease)?;
    validate_ownership_manifest_binding(
        ownership,
        expected,
        HistoryOwnershipState::Migrating,
        expected_epoch,
    )?;
    ownership.validate_writer_lease(writer_lease)
}

/// Classifies the durable marker for crash recovery while proving that the
/// caller still owns the writer lease for the same current `Migrating` epoch.
///
/// `Missing` is returned only when both the marker and target source data are
/// absent. A malformed marker, an epoch/binding mismatch, or source data with
/// no marker is an error. `Complete` describes the marker only; callers must
/// obtain [`LocalV1MigrationActivationEvidence`] before activating v2.
pub fn inspect_local_v1_migration_recovery(
    legacy: &HistoryStore,
    target: &SourceHistoryStore,
    ownership: &HistoryOwnershipStore,
    writer_lease: &HistoryWriterLease,
    expected_ownership: &HistoryOwnershipManifest,
    options: &LocalV1MigrationOptions<'_>,
) -> io::Result<LocalV1MigrationRecoveryStatus> {
    validate_migration_options(options)?;
    validate_current_migrating_ownership(
        ownership,
        writer_lease,
        expected_ownership,
        target,
        options.redaction_profile,
        options.expected_ownership_epoch,
    )?;
    let v1_namespace = validate_store_binding(legacy, target, options.redaction_profile)?;
    let imports = imports_directory(target);
    let state_path = migration_state_path(target, options.redaction_profile);
    let status = match fs::symlink_metadata(&imports) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            ensure_uninitialized_source_namespace(
                target,
                options.source_identity.node_id(),
                options.redaction_profile,
            )?;
            LocalV1MigrationRecoveryStatus::Missing
        }
        Err(error) => return Err(error),
        Ok(_) => {
            target.validate_private_path(&imports)?;
            let lock = open_lock_file(&imports)?;
            lock_shared(&lock, &imports)?;
            match read_optional_state(&state_path)? {
                Some(state) => {
                    state.validate_binding(target, options, &v1_namespace)?;
                    match state.status {
                        MigrationStatus::Running => {
                            LocalV1MigrationRecoveryStatus::Running(state.checkpoint())
                        }
                        MigrationStatus::Complete => {
                            LocalV1MigrationRecoveryStatus::Complete(state.checkpoint())
                        }
                    }
                }
                None => {
                    ensure_uninitialized_source_namespace(
                        target,
                        options.source_identity.node_id(),
                        options.redaction_profile,
                    )?;
                    LocalV1MigrationRecoveryStatus::Missing
                }
            }
        }
    };
    validate_current_migrating_ownership(
        ownership,
        writer_lease,
        expected_ownership,
        target,
        options.redaction_profile,
        options.expected_ownership_epoch,
    )?;
    Ok(status)
}

/// Revalidates a complete marker against a freshly frozen v1 snapshot and the
/// persisted v2 data while the same current `Migrating` writer lease remains
/// held. This is the only API in this module that can create activation
/// evidence after crash recovery.
pub fn verify_local_v1_migration_for_activation(
    legacy: &mut HistoryStore,
    target: &SourceHistoryStore,
    ownership: &HistoryOwnershipStore,
    writer_lease: &HistoryWriterLease,
    expected_ownership: &HistoryOwnershipManifest,
    options: &LocalV1MigrationOptions<'_>,
) -> io::Result<LocalV1MigrationActivationEvidence> {
    validate_migration_options(options)?;
    validate_current_migrating_ownership(
        ownership,
        writer_lease,
        expected_ownership,
        target,
        options.redaction_profile,
        options.expected_ownership_epoch,
    )?;
    let v1_namespace = validate_store_binding(legacy, target, options.redaction_profile)?;
    let imports = imports_directory(target);
    target.validate_private_path(&imports)?;
    let lock = open_existing_lock_file(&imports)?;
    lock_exclusive(&lock, &imports)?;
    let state = read_state(&migration_state_path(target, options.redaction_profile))?;
    state.validate_binding(target, options, &v1_namespace)?;
    verify_complete_state_with_legacy(
        legacy,
        target,
        ownership,
        writer_lease,
        expected_ownership,
        options,
        &state,
    )
}

/// Revalidates and activates v2 as one lease-protected cutover operation.
///
/// The migration lock, frozen v1 snapshot lock, and ownership writer lease
/// remain held through the final compare-and-swap, so neither a retry nor an
/// old v1 writer can create a tail between verification and `V2Active`.
pub fn activate_local_v2_history(
    legacy: &mut HistoryStore,
    target: &SourceHistoryStore,
    ownership: &HistoryOwnershipStore,
    writer_lease: &HistoryWriterLease,
    expected_ownership: &HistoryOwnershipManifest,
    options: &LocalV1MigrationOptions<'_>,
) -> io::Result<LocalV1MigrationActivation> {
    validate_migration_options(options)?;
    validate_current_migrating_ownership(
        ownership,
        writer_lease,
        expected_ownership,
        target,
        options.redaction_profile,
        options.expected_ownership_epoch,
    )?;
    let v1_namespace = validate_store_binding(legacy, target, options.redaction_profile)?;
    let imports = imports_directory(target);
    target.validate_private_path(&imports)?;
    let lock = open_existing_lock_file(&imports)?;
    lock_exclusive(&lock, &imports)?;
    let state = read_state(&migration_state_path(target, options.redaction_profile))?;
    state.validate_binding(target, options, &v1_namespace)?;
    let write_authority = ownership.authorize_v2_write(writer_lease, expected_ownership)?;
    let writer = target.writer(&write_authority)?;
    with_verified_complete_state(
        legacy,
        CompleteMigrationVerification {
            target,
            ownership,
            writer_lease,
            expected_ownership,
            options,
            state: &state,
        },
        |evidence| {
            writer.ensure_local_observation_revision_floor(
                options.source_identity,
                options.redaction_profile,
                evidence.attempt(),
            )?;
            let activated = match ownership.compare_and_transition(
                writer_lease,
                expected_ownership,
                HistoryOwnershipState::V2Active,
            )? {
                crate::history_ownership::OwnershipCasOutcome::Applied(manifest) => manifest,
                crate::history_ownership::OwnershipCasOutcome::Conflict(_) => {
                    return Err(invalid_data(
                        "history ownership changed before verified v2 activation",
                    ));
                }
            };
            if activated.state() != HistoryOwnershipState::V2Active
                || activated.epoch() != evidence.ownership_epoch()
            {
                return Err(invalid_data(
                    "activated history ownership does not preserve migration evidence",
                ));
            }
            ownership.validate_writer_lease(writer_lease)?;
            Ok(LocalV1MigrationActivation {
                evidence,
                ownership: activated,
            })
        },
    )
}

fn verify_complete_state_with_legacy(
    legacy: &mut HistoryStore,
    target: &SourceHistoryStore,
    ownership: &HistoryOwnershipStore,
    writer_lease: &HistoryWriterLease,
    expected_ownership: &HistoryOwnershipManifest,
    options: &LocalV1MigrationOptions<'_>,
    state: &MigrationState,
) -> io::Result<LocalV1MigrationActivationEvidence> {
    with_verified_complete_state(
        legacy,
        CompleteMigrationVerification {
            target,
            ownership,
            writer_lease,
            expected_ownership,
            options,
            state,
        },
        Ok,
    )
}

fn with_verified_complete_state<T>(
    legacy: &mut HistoryStore,
    context: CompleteMigrationVerification<'_, '_>,
    on_verified: impl FnOnce(LocalV1MigrationActivationEvidence) -> io::Result<T>,
) -> io::Result<T> {
    match inspect_complete_state_with_legacy(legacy, context, on_verified)? {
        CompleteMigrationInspection::Verified(value) => Ok(value),
        CompleteMigrationInspection::V1SnapshotChanged => Err(invalid_data(
            "completed local v1 migration marker does not match the frozen v1 snapshot; rerun migration before activation",
        )),
    }
}

fn inspect_complete_state_with_legacy<T>(
    legacy: &mut HistoryStore,
    context: CompleteMigrationVerification<'_, '_>,
    on_verified: impl FnOnce(LocalV1MigrationActivationEvidence) -> io::Result<T>,
) -> io::Result<CompleteMigrationInspection<T>> {
    let CompleteMigrationVerification {
        target,
        ownership,
        writer_lease,
        expected_ownership,
        options,
        state,
    } = context;
    if state.status != MigrationStatus::Complete {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "local v1 migration is not complete",
        ));
    }
    validate_current_migrating_ownership(
        ownership,
        writer_lease,
        expected_ownership,
        target,
        options.redaction_profile,
        state.expected_ownership_epoch,
    )?;
    legacy.with_exclusive_persisted_snapshot_since(state.window_starts_at, |snapshot| {
        validate_snapshot(snapshot)?;
        ensure_import_bounds(snapshot)?;
        let current_digest = frozen_v1_digest(snapshot)?;
        if state.frozen_v1_digest.as_deref() != Some(current_digest.as_str()) {
            return Ok(CompleteMigrationInspection::V1SnapshotChanged);
        }
        state.validate_complete_snapshot_counts(snapshot)?;
        verify_account_snapshot(target, state.window_starts_at, snapshot)?;
        verify_source_snapshot(
            target,
            options.source_identity.node_id(),
            options.redaction_profile,
            state.window_starts_at,
            snapshot,
        )?;
        validate_current_migrating_ownership(
            ownership,
            writer_lease,
            expected_ownership,
            target,
            options.redaction_profile,
            state.expected_ownership_epoch,
        )?;
        let evidence = LocalV1MigrationActivationEvidence {
            ownership_epoch: state.expected_ownership_epoch,
            attempt: state.attempt,
            completed_at: state.completed_at.ok_or_else(|| {
                invalid_data("complete local v1 migration state lacks its completion time")
            })?,
        };
        on_verified(evidence).map(CompleteMigrationInspection::Verified)
    })
}

fn verify_account_snapshot(
    target: &SourceHistoryStore,
    since: DateTime<Utc>,
    expected: &HistoryData,
) -> io::Result<()> {
    let loaded = target.load_account_since(since)?;
    for expected_point in &expected.quota_points {
        if !loaded
            .quota_points
            .iter()
            .any(|point| point == expected_point)
        {
            return Err(invalid_data(format!(
                "v2 account history did not preserve the imported quota point observed at {}",
                expected_point.observed_at.to_rfc3339()
            )));
        }
    }
    Ok(())
}

/// Reads only the account and source namespaces covered by a completed local
/// v1 import. The two domains remain separate so callers cannot accidentally
/// count account quota as source-local usage.
pub fn load_migrated_local_history_since(
    target: &SourceHistoryStore,
    ownership: &HistoryOwnershipStore,
    expected_ownership: &HistoryOwnershipManifest,
    identity: &SourceIdentity,
    redaction_profile: RedactionProfile,
    since: DateTime<Utc>,
) -> io::Result<MigratedLocalHistory> {
    validate_current_query_ownership(ownership, expected_ownership, target, redaction_profile)?;
    let imports_directory = imports_directory(target);
    target.validate_private_path(&imports_directory)?;
    let lock = open_existing_lock_file(&imports_directory)?;
    lock_shared(&lock, &imports_directory)?;
    let state = read_state(&migration_state_path(target, redaction_profile))?;
    state.validate_query_binding(
        target,
        identity,
        redaction_profile,
        expected_ownership.epoch(),
    )?;
    if state.status != MigrationStatus::Complete {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "local v1 migration is not complete",
        ));
    }

    let migrated = MigratedLocalHistory {
        migration_window_starts_at: state.window_starts_at,
        account: target.load_account_since(since)?,
        source: target.load_source_since(identity.node_id(), redaction_profile, since)?,
    };
    validate_current_query_ownership(ownership, expected_ownership, target, redaction_profile)?;
    Ok(migrated)
}

fn validate_current_query_ownership(
    ownership: &HistoryOwnershipStore,
    expected: &HistoryOwnershipManifest,
    target: &SourceHistoryStore,
    redaction_profile: RedactionProfile,
) -> io::Result<()> {
    validate_ownership_store_binding(ownership, target, redaction_profile)?;
    if !matches!(
        expected.state(),
        HistoryOwnershipState::Migrating | HistoryOwnershipState::V2Active
    ) || expected.epoch() <= 1
    {
        return Err(invalid_data(
            "migrated history query requires a cutover ownership manifest",
        ));
    }
    validate_ownership_manifest_binding(ownership, expected, expected.state(), expected.epoch())
}

fn validate_store_binding(
    legacy: &HistoryStore,
    target: &SourceHistoryStore,
    redaction_profile: RedactionProfile,
) -> io::Result<String> {
    let legacy_root = legacy.history_root().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "v1 history state directory is unavailable",
        )
    })?;
    if legacy_root.file_name() != Some(OsStr::new(LEGACY_HISTORY_DIRECTORY))
        || legacy_root.parent() != Some(target.state_root())
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "v1 history must be the history-v1 child of the v2 state root",
        ));
    }
    let redacted = redaction_profile == RedactionProfile::Redacted;
    if legacy.redact_content_enabled() != redacted {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "v1 history redaction mode does not match the v2 namespace",
        ));
    }
    let profile = if redacted {
        legacy
            .namespace()
            .strip_suffix("-redacted")
            .ok_or_else(|| {
                invalid_data("redacted v1 history namespace lacks its redaction suffix")
            })?
    } else {
        legacy.namespace()
    };
    if profile != target.profile_id().as_str() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "v1 history namespace does not match the v2 profile",
        ));
    }
    Ok(legacy.namespace().to_owned())
}

fn validate_snapshot(snapshot: &HistoryData) -> io::Result<()> {
    if snapshot.read_only {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "v1 history is read-only and cannot be marked as migrated",
        ));
    }
    if !snapshot.warnings.is_empty() {
        return Err(invalid_data(format!(
            "v1 history could not be imported without data loss: {}",
            snapshot.warnings.join("; ")
        )));
    }
    Ok(())
}

struct SnapshotDigestWriter<'a>(&'a mut Sha256);

impl Write for SnapshotDigestWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Hash only the three durable v1 vectors imported by this bridge. Query
/// warnings, read-only state, and Summary's backfill marker are deliberately
/// excluded because they are not copied into the source-aware history.
fn frozen_v1_digest(snapshot: &HistoryData) -> io::Result<String> {
    let mut digest = Sha256::new();
    digest.update(FROZEN_V1_DIGEST_DOMAIN);
    serde_json::to_writer(
        SnapshotDigestWriter(&mut digest),
        &(
            &snapshot.quota_points,
            &snapshot.half_hour_buckets,
            &snapshot.weekly_local_points,
        ),
    )
    .map_err(|error| invalid_data(format!("could not hash frozen v1 snapshot: {error}")))?;
    Ok(format!("{:x}", digest.finalize()))
}

fn valid_frozen_v1_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .as_bytes()
            .iter()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
}

fn ensure_import_bounds(snapshot: &HistoryData) -> io::Result<()> {
    if snapshot.quota_points.len() > MAX_IMPORTED_QUOTA_POINTS {
        return Err(invalid_data(
            "v1 quota-point import exceeds its safety bound",
        ));
    }
    if snapshot.half_hour_buckets.len() > MAX_IMPORTED_BUCKETS {
        return Err(invalid_data("v1 bucket import exceeds its safety bound"));
    }
    if snapshot.weekly_local_points.len() > MAX_IMPORTED_WEEKLY_POINTS {
        return Err(invalid_data(
            "v1 weekly-point import exceeds its safety bound",
        ));
    }
    Ok(())
}

fn ensure_local_source(
    target: &SourceHistoryStore,
    writer: &SourceHistoryWriter<'_, '_, '_>,
    proposed: &SourceMetadata,
) -> io::Result<()> {
    match target.load_source_metadata(proposed.source_id()) {
        Ok(existing) => reconcile_existing_local_source(writer, existing, proposed),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            match writer.save_source_metadata(proposed) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    reconcile_existing_local_source(
                        writer,
                        target.load_source_metadata(proposed.source_id())?,
                        proposed,
                    )
                }
                Err(error) => Err(error),
            }
        }
        Err(error) => Err(error),
    }
}

fn reconcile_existing_local_source(
    writer: &SourceHistoryWriter<'_, '_, '_>,
    existing: SourceMetadata,
    proposed: &SourceMetadata,
) -> io::Result<()> {
    validate_existing_local_source(&existing)?;
    if existing.display_label() != proposed.display_label() {
        return Err(invalid_data(
            "existing local source label does not match the v1 migration source",
        ));
    }
    if existing.aggregate_redaction_profile() == proposed.aggregate_redaction_profile() {
        return Ok(());
    }
    writer
        .update_source_metadata(proposed.source_id(), |metadata| {
            validate_existing_local_source(metadata)?;
            if metadata.display_label() != proposed.display_label() {
                return Err(invalid_data(
                    "existing local source label changed during v1 migration",
                ));
            }
            metadata.set_aggregate_redaction_profile(proposed.aggregate_redaction_profile());
            Ok(())
        })
        .map(|_| ())
}

fn validate_existing_migration_revisions(
    target: &SourceHistoryStore,
    source_id: &NodeId,
    redaction_profile: RedactionProfile,
    since: DateTime<Utc>,
    current_attempt: u64,
) -> io::Result<()> {
    match target.load_source_metadata(source_id) {
        Ok(metadata) => validate_existing_local_source(&metadata)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            ensure_uninitialized_source_namespace(target, source_id, redaction_profile)?;
            return Ok(());
        }
        Err(error) => return Err(error),
    }
    let records = target.load_source_records_since(source_id, redaction_profile, since)?;
    if records
        .records
        .iter()
        .any(|record| record.revision() > current_attempt)
        || records
            .weekly_records
            .iter()
            .any(|record| record.revision() > current_attempt)
    {
        return Err(invalid_data(
            "migration source contains a revision not owned by the current or an earlier attempt",
        ));
    }
    Ok(())
}

fn reconcile_migration_source_changes(
    target: &SourceHistoryStore,
    source_id: &NodeId,
    redaction_profile: RedactionProfile,
    since: DateTime<Utc>,
    attempt: u64,
    snapshot: &HistoryData,
) -> io::Result<(Vec<SourceBucketRecord>, Vec<SourceWeeklyRecord>)> {
    let existing = target.load_source_records_since(source_id, redaction_profile, since)?;
    if existing
        .records
        .iter()
        .any(|record| record.revision() >= attempt)
        || existing
            .weekly_records
            .iter()
            .any(|record| record.revision() >= attempt)
    {
        return Err(invalid_data(
            "migration source contains a revision not owned by an earlier attempt",
        ));
    }

    let expected_bucket_keys = snapshot
        .half_hour_buckets
        .iter()
        .map(|bucket| bucket.starts_at)
        .collect::<BTreeSet<_>>();
    let mut bucket_changes = Vec::with_capacity(
        snapshot
            .half_hour_buckets
            .len()
            .saturating_add(existing.records.len()),
    );
    for record in existing.records {
        if matches!(record.change(), SourceBucketChange::Upsert(_))
            && !expected_bucket_keys.contains(&record.starts_at())
        {
            bucket_changes.push(SourceBucketRecord::tombstone(record.starts_at(), attempt)?);
        }
    }
    bucket_changes.extend(
        snapshot
            .half_hour_buckets
            .iter()
            .cloned()
            .map(|bucket| SourceBucketRecord::upsert(attempt, bucket))
            .collect::<io::Result<Vec<_>>>()?,
    );
    bucket_changes.sort_by_key(SourceBucketRecord::starts_at);

    let expected_weekly_keys = snapshot
        .weekly_local_points
        .iter()
        .map(|point| (point.observed_at, point.resets_at))
        .collect::<BTreeSet<_>>();
    let mut weekly_changes = Vec::with_capacity(
        snapshot
            .weekly_local_points
            .len()
            .saturating_add(existing.weekly_records.len()),
    );
    for record in existing.weekly_records {
        if matches!(record.change(), SourceWeeklyChange::Upsert(_))
            && !expected_weekly_keys.contains(&(record.observed_at(), record.resets_at()))
        {
            weekly_changes.push(SourceWeeklyRecord::tombstone(
                record.observed_at(),
                record.resets_at(),
                attempt,
            )?);
        }
    }
    weekly_changes.extend(
        snapshot
            .weekly_local_points
            .iter()
            .cloned()
            .map(|point| SourceWeeklyRecord::upsert(attempt, point))
            .collect::<io::Result<Vec<_>>>()?,
    );
    weekly_changes.sort_by_key(|record| (record.observed_at(), record.resets_at()));
    Ok((bucket_changes, weekly_changes))
}

fn ensure_uninitialized_source_namespace(
    target: &SourceHistoryStore,
    source_id: &NodeId,
    redaction_profile: RedactionProfile,
) -> io::Result<()> {
    for path in [
        target.source_buckets_directory(source_id, redaction_profile),
        target.source_weekly_directory(source_id, redaction_profile),
        target.source_digests_directory(source_id, redaction_profile),
        target.source_facts_directory(source_id, redaction_profile),
        target.source_fact_manifests_directory(source_id, redaction_profile),
        target.source_fact_staging_directory(source_id, redaction_profile),
    ] {
        match fs::symlink_metadata(&path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Ok(_) => {
                // Fail closed on an existing link, reparse point, file, or
                // private directory. Without a matching marker no selected-
                // profile v2 evidence can safely be claimed by this import.
                target.validate_private_path(&path)?;
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "local source history already exists without a matching migration state",
                ));
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn verify_source_snapshot(
    target: &SourceHistoryStore,
    source_id: &NodeId,
    redaction_profile: RedactionProfile,
    since: DateTime<Utc>,
    expected: &HistoryData,
) -> io::Result<()> {
    let loaded = target.load_source_since(source_id, redaction_profile, since)?;
    let mut expected_buckets = expected.half_hour_buckets.clone();
    expected_buckets.sort_by_key(|bucket| bucket.starts_at);
    if loaded.buckets != expected_buckets {
        return Err(invalid_data(
            "v2 local source does not exactly match the imported bucket snapshot",
        ));
    }
    let mut expected_weekly = expected.weekly_local_points.clone();
    expected_weekly.sort_by_key(|point| (point.observed_at, point.resets_at));
    if loaded.weekly_local_points != expected_weekly {
        return Err(invalid_data(
            "v2 local source does not exactly match the imported weekly snapshot",
        ));
    }
    Ok(())
}

fn validate_existing_local_source(metadata: &SourceMetadata) -> io::Result<()> {
    if metadata.kind() == SourceKind::Local {
        Ok(())
    } else {
        Err(invalid_data(
            "the local migration node ID is already registered as a non-local source",
        ))
    }
}

fn report_from_complete_state(
    state: &MigrationState,
    outcome: LocalV1MigrationOutcome,
) -> LocalV1MigrationReport {
    LocalV1MigrationReport {
        outcome,
        attempt: state.attempt,
        quota_points: state.quota_points.unwrap_or(0),
        buckets: state.buckets.unwrap_or(0),
        weekly_local_points: state.weekly_local_points.unwrap_or(0),
        account_shards_written: 0,
        source_shards_written: 0,
        source_weekly_shards_written: 0,
        v1_writer_cutover_required: true,
    }
}

fn imports_directory(store: &SourceHistoryStore) -> PathBuf {
    store.profile_directory().join(IMPORTS_DIRECTORY)
}

fn migration_state_path(
    store: &SourceHistoryStore,
    redaction_profile: RedactionProfile,
) -> PathBuf {
    imports_directory(store).join(format!(
        "{MIGRATION_STATE_PREFIX}{}.json",
        redaction_profile.directory_name()
    ))
}

fn read_optional_state(path: &Path) -> io::Result<Option<MigrationState>> {
    match read_state(path) {
        Ok(state) => Ok(Some(state)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn read_state(path: &Path) -> io::Result<MigrationState> {
    let path_metadata = fs::symlink_metadata(path)?;
    validate_data_file_metadata(&path_metadata)?;
    let mut options = OpenOptions::new();
    options.read(true);
    add_nofollow_flags(&mut options);
    let file = options
        .open(path)
        .map_err(|error| map_nofollow_error(error, "local v1 migration state"))?;
    let metadata = file.metadata()?;
    validate_data_file_metadata(&metadata)?;
    ensure_opened_file_matches_path(
        path,
        &file,
        &path_metadata,
        &metadata,
        "local v1 migration state",
    )?;
    if metadata.len() > MAX_STATE_FILE_BYTES {
        return Err(invalid_data("local v1 migration state is too large"));
    }
    let mut contents = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_STATE_FILE_BYTES + 1)
        .read_to_end(&mut contents)?;
    if contents.len() as u64 > MAX_STATE_FILE_BYTES {
        return Err(invalid_data("local v1 migration state is too large"));
    }
    let state: MigrationState = serde_json::from_slice(&contents)
        .map_err(|error| invalid_data(format!("invalid local v1 migration state: {error}")))?;
    state.validate()?;
    Ok(state)
}

fn write_state(path: &Path, state: &MigrationState) -> io::Result<()> {
    state.validate()?;
    let mut contents = serde_json::to_vec_pretty(state)
        .map_err(|error| invalid_data(format!("could not encode migration state: {error}")))?;
    contents.push(b'\n');
    if contents.len() as u64 > MAX_STATE_FILE_BYTES {
        return Err(invalid_data(
            "encoded local v1 migration state is too large",
        ));
    }
    write_private_atomically(path, &contents)
}

fn write_private_atomically(path: &Path, contents: &[u8]) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    create_private_directory(parent)?;
    let file_name = path
        .file_name()
        .unwrap_or_else(|| OsStr::new("local-v1-migration"));
    let (temporary, mut file) = create_temporary_file(parent, file_name)?;
    let result = (|| {
        file.write_all(contents)?;
        file.sync_all()?;
        drop(file);
        replace_file(&temporary, path)?;
        validate_published_private_file(path)?;
        sync_directory(parent)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn create_temporary_file(parent: &Path, file_name: &OsStr) -> io::Result<(PathBuf, File)> {
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
        match options.open(&temporary) {
            Ok(file) => {
                let validation = (|| {
                    let metadata = file.metadata()?;
                    validate_data_file_metadata(&metadata)?;
                    #[cfg(windows)]
                    validate_windows_private_file(
                        &temporary,
                        &file,
                        "local v1 migration temporary file",
                    )?;
                    Ok(())
                })();
                match validation {
                    Ok(()) => return Ok((temporary, file)),
                    Err(error) => {
                        drop(file);
                        let _ = fs::remove_file(&temporary);
                        return Err(error);
                    }
                }
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a local v1 migration temporary file",
    ))
}

fn create_private_directory(path: &Path) -> io::Result<()> {
    match validate_private_directory(path) {
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
    {
        #[cfg(windows)]
        reject_windows_reparse_components_before_create(
            path,
            "local v1 migration state directory",
        )?;
        fs::create_dir_all(path)?;
    }
    validate_private_directory(path)
}

fn validate_private_directory(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata_is_link_or_reparse(&metadata) || !metadata.file_type().is_dir() {
        return Err(invalid_data(
            "local v1 migration state path must be a real directory",
        ));
    }
    ensure_private_directory(&metadata)?;
    #[cfg(windows)]
    validate_windows_private_directory(path, "local v1 migration state directory")?;
    Ok(())
}

fn validate_published_private_file(path: &Path) -> io::Result<()> {
    let path_metadata = fs::symlink_metadata(path)?;
    validate_data_file_metadata(&path_metadata)?;
    let mut options = OpenOptions::new();
    options.read(true);
    add_nofollow_flags(&mut options);
    let file = options
        .open(path)
        .map_err(|error| map_nofollow_error(error, "local v1 migration state"))?;
    let metadata = file.metadata()?;
    validate_data_file_metadata(&metadata)?;
    ensure_opened_file_matches_path(
        path,
        &file,
        &path_metadata,
        &metadata,
        "local v1 migration published state",
    )
}

fn open_lock_file(directory: &Path) -> io::Result<File> {
    open_lock_file_with_create(directory, true)
}

fn open_existing_lock_file(directory: &Path) -> io::Result<File> {
    open_lock_file_with_create(directory, false)
}

fn open_lock_file_with_create(directory: &Path, create: bool) -> io::Result<File> {
    validate_private_directory(directory)?;
    let path = directory.join(MIGRATION_LOCK_FILE);
    match fs::symlink_metadata(&path) {
        Ok(metadata) => validate_lock_metadata(&metadata)?,
        Err(error) if create && error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(create);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.share_mode(stable_lock_share_mode());
    }
    add_nofollow_flags(&mut options);
    let file = options
        .open(&path)
        .map_err(|error| map_nofollow_error(error, "local v1 migration lock"))?;
    let metadata = file.metadata()?;
    validate_lock_metadata(&metadata)?;
    let path_metadata = fs::symlink_metadata(&path)?;
    validate_lock_metadata(&path_metadata)?;
    ensure_opened_file_matches_path(
        &path,
        &file,
        &path_metadata,
        &metadata,
        "local v1 migration lock",
    )?;
    Ok(file)
}

fn lock_exclusive(file: &File, directory: &Path) -> io::Result<()> {
    fs2::FileExt::lock_exclusive(file)?;
    validate_locked_file(file, directory)
}

fn lock_shared(file: &File, directory: &Path) -> io::Result<()> {
    fs2::FileExt::lock_shared(file)?;
    validate_locked_file(file, directory)
}

fn validate_locked_file(file: &File, directory: &Path) -> io::Result<()> {
    validate_private_directory(directory)?;
    let path = directory.join(MIGRATION_LOCK_FILE);
    let path_metadata = fs::symlink_metadata(&path)?;
    validate_lock_metadata(&path_metadata)?;
    let opened_metadata = file.metadata()?;
    validate_lock_metadata(&opened_metadata)?;
    ensure_opened_file_matches_path(
        &path,
        file,
        &path_metadata,
        &opened_metadata,
        "local v1 migration lock",
    )
}

fn validate_lock_metadata(metadata: &fs::Metadata) -> io::Result<()> {
    if metadata_is_link_or_reparse(metadata) || !metadata.file_type().is_file() {
        return Err(invalid_data(
            "local v1 migration lock must be a regular file",
        ));
    }
    ensure_private_file(metadata, "local v1 migration lock")
}

fn validate_data_file_metadata(metadata: &fs::Metadata) -> io::Result<()> {
    if metadata_is_link_or_reparse(metadata) || !metadata.file_type().is_file() {
        return Err(invalid_data(
            "local v1 migration state must be a regular file",
        ));
    }
    ensure_private_file(metadata, "local v1 migration state")
}

#[cfg(unix)]
fn metadata_is_link_or_reparse(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(windows)]
fn metadata_is_link_or_reparse(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

    metadata.file_type().is_symlink()
        || windows_attributes_are_reparse(metadata.file_attributes(), FILE_ATTRIBUTE_REPARSE_POINT)
}

#[cfg(not(any(unix, windows)))]
fn metadata_is_link_or_reparse(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(any(windows, test))]
fn windows_attributes_are_reparse(attributes: u32, reparse_flag: u32) -> bool {
    attributes & reparse_flag != 0
}

#[cfg(windows)]
fn stable_lock_share_mode() -> u32 {
    use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};

    FILE_SHARE_READ | FILE_SHARE_WRITE
}

#[cfg(test)]
fn stable_lock_share_mode_for_test() -> u32 {
    0x1 | 0x2
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
fn ensure_private_directory(metadata: &fs::Metadata) -> io::Result<()> {
    ensure_private_unix(metadata, "local v1 migration state directory")
}

#[cfg(unix)]
fn ensure_private_file(metadata: &fs::Metadata, subject: &str) -> io::Result<()> {
    ensure_private_unix(metadata, subject)
}

#[cfg(unix)]
fn ensure_private_unix(metadata: &fs::Metadata, subject: &str) -> io::Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    // SAFETY: geteuid has no preconditions and does not retain pointers.
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
fn ensure_private_directory(_metadata: &fs::Metadata) -> io::Result<()> {
    Ok(())
}

#[cfg(not(unix))]
fn ensure_private_file(_metadata: &fs::Metadata, _subject: &str) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn ensure_opened_file_matches_path(
    _path: &Path,
    _opened_file: &File,
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
    opened_file: &File,
    _path_metadata: &fs::Metadata,
    _opened_metadata: &fs::Metadata,
    subject: &str,
) -> io::Result<()> {
    validate_windows_private_file(path, opened_file, subject)?;
    let expected = windows_file_identity(opened_file, subject)?;
    let mut options = OpenOptions::new();
    options.read(true);
    add_nofollow_flags(&mut options);
    let current = options
        .open(path)
        .map_err(|error| map_nofollow_error(error, subject))?;
    validate_windows_private_file(path, &current, subject)?;
    if windows_file_identity(&current, subject)? == expected {
        Ok(())
    } else {
        Err(invalid_data(format!(
            "{subject} changed while it was being opened"
        )))
    }
}

#[cfg(windows)]
fn reject_windows_reparse_components_before_create(path: &Path, subject: &str) -> io::Result<()> {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 => {
                return Err(invalid_data(format!(
                    "{subject} path must not traverse a reparse point ({})",
                    current.display()
                )));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => break,
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[cfg(windows)]
fn windows_file_identity(file: &File, subject: &str) -> io::Result<(u32, u64)> {
    use std::mem::MaybeUninit;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };

    let mut information = MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::zeroed();
    // SAFETY: the live handle and output pointer remain valid for this call.
    let success =
        unsafe { GetFileInformationByHandle(file.as_raw_handle(), information.as_mut_ptr()) };
    if success == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the API reported success and initialized the full structure.
    let information = unsafe { information.assume_init() };
    let file_index =
        (u64::from(information.nFileIndexHigh) << 32) | u64::from(information.nFileIndexLow);
    require_stable_file_identity(
        Some(information.dwVolumeSerialNumber),
        Some(file_index),
        subject,
    )
}

#[cfg(any(windows, test))]
fn require_stable_file_identity(
    volume_serial_number: Option<u32>,
    file_index: Option<u64>,
    subject: &str,
) -> io::Result<(u32, u64)> {
    match (volume_serial_number, file_index) {
        (Some(volume_serial_number), Some(file_index)) => Ok((volume_serial_number, file_index)),
        _ => Err(invalid_data(format!(
            "{subject} does not expose a stable file identity"
        ))),
    }
}

#[cfg(not(any(unix, windows)))]
fn ensure_opened_file_matches_path(
    _path: &Path,
    _opened_file: &File,
    _path_metadata: &fs::Metadata,
    _opened_metadata: &fs::Metadata,
    _subject: &str,
) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> io::Result<()> {
    Ok(())
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, TimeZone};
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration as StdDuration;
    use tempfile::tempdir;

    use crate::api_cost::API_PRICING_CATALOG_REVISION;
    use crate::domain::{ApiCostAmount, Provenance, TokenUsage};
    use crate::history::{
        HISTORY_ESTIMATOR_REVISION, HISTORY_PROJECT_BREAKDOWN_REVISION, HistoryObservation,
        LocalHalfHourBucket, LocalProjectUsageGroup, QuotaPoint, WeeklyLocalPoint,
    };
    use crate::source_identity::SourceIdentityStore;

    const MIGRATION_EPOCH: u64 = 2;

    fn at(day: u32, hour: u32, minute: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, day, hour, minute, 0)
            .single()
            .unwrap()
    }

    fn bucket(starts_at: DateTime<Utc>, input_tokens: u64) -> LocalHalfHourBucket {
        let ends_at = starts_at + Duration::minutes(15);
        LocalHalfHourBucket {
            starts_at,
            ends_at,
            sampled_at: ends_at,
            token_usage: TokenUsage {
                input_tokens,
                ..TokenUsage::default()
            },
            estimated_cost_units: u128::from(input_tokens),
            api_long_context_extra_cost_units: Some(0),
            long_context_usage_unknown: false,
            estimator_revision: HISTORY_ESTIMATOR_REVISION,
            project_breakdown_revision: HISTORY_PROJECT_BREAKDOWN_REVISION,
            api_pricing_catalog_revision: API_PRICING_CATALOG_REVISION,
            call_count: 1,
            groups: Vec::new(),
            project_groups: vec![LocalProjectUsageGroup {
                thread_id: "thread-1".to_string(),
                project_id: Some("project-1".to_string()),
                project_label: Some("project".to_string()),
                title: Some("private title".to_string()),
                message_preview: Some("private prompt".to_string()),
                token_usage: TokenUsage {
                    input_tokens,
                    ..TokenUsage::default()
                },
                estimated_cost_units: u128::from(input_tokens),
                api_equivalent_cost: ApiCostAmount::default(),
                call_count: 1,
                ..LocalProjectUsageGroup::default()
            }],
            partial_reasons: Vec::new(),
        }
    }

    fn quota_point(observed_at: DateTime<Utc>) -> QuotaPoint {
        QuotaPoint {
            observed_at,
            limit_id: "codex".to_string(),
            duration_mins: 10_080,
            resets_at: observed_at + Duration::days(3),
            used_percent: 25.0,
            remaining_percent: 75.0,
            provenance: Provenance::ServerSnapshot,
        }
    }

    fn weekly_point(observed_at: DateTime<Utc>) -> WeeklyLocalPoint {
        WeeklyLocalPoint {
            observed_at,
            resets_at: observed_at + Duration::days(3),
            token_usage: TokenUsage {
                input_tokens: 77,
                total_tokens: 77,
                ..TokenUsage::default()
            },
            estimated_cost_units: 1234,
            api_long_context_extra_cost_units: Some(567),
            long_context_usage_unknown: false,
            estimator_revision: HISTORY_ESTIMATOR_REVISION,
            call_count: 4,
            partial_reasons: vec!["rollout_scan_incomplete".to_string()],
        }
    }

    fn identity(directory: &Path) -> SourceIdentity {
        SourceIdentityStore::at_path(directory.join("identity/source-identity.json"))
            .load_or_create()
            .unwrap()
    }

    fn preview_stores(directory: &Path) -> (HistoryStore, SourceHistoryStore, PathBuf, PathBuf) {
        let state_root = directory.join("state");
        let legacy_root = state_root.join(LEGACY_HISTORY_DIRECTORY);
        let codex_home = directory.join("codex-home");
        let legacy = HistoryStore::new(legacy_root, &codex_home);
        let profile_id = legacy.namespace().parse::<HistoryProfileId>().unwrap();
        let target = SourceHistoryStore::new(state_root.clone(), profile_id);
        (legacy, target, state_root, codex_home)
    }

    fn migrating_ownership(
        target: &SourceHistoryStore,
        redaction_profile: RedactionProfile,
    ) -> (
        HistoryOwnershipStore,
        HistoryWriterLease,
        HistoryOwnershipManifest,
    ) {
        let store = HistoryOwnershipStore::new(
            target.state_root().to_path_buf(),
            target.profile_id().clone(),
            redaction_profile,
        );
        let lease = match store.try_acquire_writer_lease().unwrap() {
            crate::history_ownership::TryWriterLease::Acquired(lease) => lease,
            crate::history_ownership::TryWriterLease::Busy(_) => {
                panic!("ownership writer lease unexpectedly busy")
            }
        };
        let current = match store.load_manifest().unwrap() {
            OwnershipManifestStatus::Uninitialized => {
                let v1 = match store.initialize_v1_active(&lease).unwrap() {
                    crate::history_ownership::InitializeV1Outcome::Initialized(manifest)
                    | crate::history_ownership::InitializeV1Outcome::Existing(manifest) => manifest,
                };
                match store.begin_migration(&lease, &v1).unwrap() {
                    crate::history_ownership::OwnershipCasOutcome::Applied(manifest) => manifest,
                    crate::history_ownership::OwnershipCasOutcome::Conflict(_) => {
                        panic!("ownership migration transition unexpectedly conflicted")
                    }
                }
            }
            OwnershipManifestStatus::Initialized(manifest)
                if manifest.state() == HistoryOwnershipState::Migrating =>
            {
                manifest
            }
            OwnershipManifestStatus::Initialized(manifest) => {
                panic!("unexpected ownership state {:?}", manifest.state())
            }
        };
        assert_eq!(current.epoch(), MIGRATION_EPOCH);
        (store, lease, current)
    }

    fn run_migration(
        legacy: &mut HistoryStore,
        target: &SourceHistoryStore,
        options: &LocalV1MigrationOptions<'_>,
    ) -> io::Result<LocalV1MigrationReport> {
        let (ownership, lease, manifest) = migrating_ownership(target, options.redaction_profile);
        migrate_local_v1_history(legacy, target, &ownership, &lease, &manifest, options)
    }

    fn load_migrated(
        target: &SourceHistoryStore,
        identity: &SourceIdentity,
        redaction_profile: RedactionProfile,
        since: DateTime<Utc>,
    ) -> io::Result<MigratedLocalHistory> {
        let ownership = HistoryOwnershipStore::new(
            target.state_root().to_path_buf(),
            target.profile_id().clone(),
            redaction_profile,
        );
        let manifest = match ownership.load_manifest()? {
            OwnershipManifestStatus::Initialized(manifest) => manifest,
            OwnershipManifestStatus::Uninitialized => {
                return Err(invalid_data("test ownership is uninitialized"));
            }
        };
        load_migrated_local_history_since(
            target,
            &ownership,
            &manifest,
            identity,
            redaction_profile,
            since,
        )
    }

    fn write_private_test_file(path: &Path, contents: &[u8]) {
        fs::write(path, contents).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = fs::metadata(path).unwrap().permissions();
            permissions.set_mode(0o600);
            fs::set_permissions(path, permissions).unwrap();
        }
    }

    fn record_legacy_sample(legacy: &mut HistoryStore, starts_at: DateTime<Utc>, tokens: u64) {
        legacy
            .record(&HistoryObservation {
                observed_at: starts_at + Duration::minutes(20),
                quota_points: vec![quota_point(starts_at + Duration::minutes(5))],
                half_hour_buckets: vec![bucket(starts_at, tokens)],
                weekly_local_points: Vec::new(),
            })
            .unwrap();
    }

    fn seed_running_attempt(
        legacy: &HistoryStore,
        target: &SourceHistoryStore,
        options: &LocalV1MigrationOptions<'_>,
        attempt: u64,
    ) {
        assert!(attempt > 0);
        let imports = imports_directory(target);
        create_private_directory(&imports).unwrap();
        let mut state = MigrationState::first_attempt(
            target.profile_id().clone(),
            legacy.namespace().to_string(),
            options,
        );
        while state.attempt < attempt {
            state.start_retry().unwrap();
        }
        write_state(
            &migration_state_path(target, options.redaction_profile),
            &state,
        )
        .unwrap();
        target
            .save_source_metadata(
                &SourceMetadata::new(
                    options.source_identity.node_id().clone(),
                    SourceKind::Local,
                    options.source_label,
                )
                .unwrap(),
            )
            .unwrap();
    }

    #[test]
    fn store_binding_requires_sibling_history_v1_and_history_v2_layouts() {
        let directory = tempdir().unwrap();
        let (legacy, target, state_root, codex_home) = preview_stores(directory.path());
        assert_eq!(
            legacy.history_root(),
            Some(state_root.join(LEGACY_HISTORY_DIRECTORY).as_path())
        );
        assert_eq!(target.layout_root(), state_root.join("history-v2"));
        assert!(validate_store_binding(&legacy, &target, RedactionProfile::PreviewEnabled).is_ok());

        let wrongly_nested = HistoryStore::new(state_root, &codex_home);
        let error =
            validate_store_binding(&wrongly_nested, &target, RedactionProfile::PreviewEnabled)
                .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn stable_file_identity_requires_both_components() {
        assert_eq!(
            require_stable_file_identity(Some(7), Some(11), "test file").unwrap(),
            (7, 11)
        );
        for (volume, index) in [(None, Some(11)), (Some(7), None), (None, None)] {
            let error = require_stable_file_identity(volume, index, "test file").unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        }
    }

    #[test]
    fn exclusive_snapshot_holds_the_v1_writer_lock_for_its_consumer() {
        let directory = tempdir().unwrap();
        let (mut legacy, _, state_root, codex_home) = preview_stores(directory.path());
        let starts_at = at(29, 8, 0);
        record_legacy_sample(&mut legacy, starts_at, 1);
        let legacy_root = state_root.join(LEGACY_HISTORY_DIRECTORY);
        let (started_tx, started_rx) = mpsc::channel();
        let (finished_tx, finished_rx) = mpsc::channel();
        let mut writer = None;

        legacy
            .with_exclusive_persisted_snapshot_since(starts_at - Duration::minutes(1), |_| {
                writer = Some(thread::spawn(move || {
                    let mut store = HistoryStore::new(legacy_root, &codex_home);
                    started_tx.send(()).unwrap();
                    record_legacy_sample(&mut store, starts_at + Duration::minutes(15), 2);
                    finished_tx.send(()).unwrap();
                }));
                started_rx.recv_timeout(StdDuration::from_secs(1)).unwrap();
                assert!(
                    finished_rx
                        .recv_timeout(StdDuration::from_millis(50))
                        .is_err()
                );
                Ok(())
            })
            .unwrap();

        finished_rx.recv_timeout(StdDuration::from_secs(1)).unwrap();
        writer.take().unwrap().join().unwrap();
    }

    #[test]
    fn imports_account_and_local_source_once_then_queries_them_separately() {
        let directory = tempdir().unwrap();
        let (mut legacy, target, _, _) = preview_stores(directory.path());
        let starts_at = at(29, 10, 0);
        record_legacy_sample(&mut legacy, starts_at, 42);
        let weekly = weekly_point(starts_at + Duration::minutes(30));
        let mut next_cycle = weekly.clone();
        next_cycle.resets_at += Duration::days(7);
        legacy
            .record(&HistoryObservation {
                observed_at: weekly.observed_at,
                quota_points: Vec::new(),
                half_hour_buckets: Vec::new(),
                weekly_local_points: vec![weekly.clone(), next_cycle.clone()],
            })
            .unwrap();
        let identity = identity(directory.path());
        let options = LocalV1MigrationOptions {
            source_identity: &identity,
            redaction_profile: RedactionProfile::PreviewEnabled,
            source_label: "local machine",
            expected_ownership_epoch: MIGRATION_EPOCH,
            window_starts_at: starts_at - Duration::hours(1),
            completed_at: at(29, 11, 0),
        };

        let first = run_migration(&mut legacy, &target, &options).unwrap();
        assert_eq!(first.outcome, LocalV1MigrationOutcome::Imported);
        assert_eq!(first.attempt, 1);
        assert_eq!((first.quota_points, first.buckets), (1, 1));
        assert_eq!(first.weekly_local_points, 2);
        assert!(first.v1_writer_cutover_required);
        assert_eq!(
            read_state(&migration_state_path(
                &target,
                RedactionProfile::PreviewEnabled
            ))
            .unwrap()
            .expected_ownership_epoch,
            MIGRATION_EPOCH
        );

        let second = run_migration(&mut legacy, &target, &options).unwrap();
        assert_eq!(second.outcome, LocalV1MigrationOutcome::AlreadyComplete);
        assert_eq!(second.attempt, 1);
        assert_eq!((second.quota_points, second.buckets), (1, 1));
        assert_eq!(second.weekly_local_points, 2);
        assert_eq!(second.account_shards_written, 0);
        assert_eq!(second.source_shards_written, 0);

        let loaded = load_migrated(
            &target,
            &identity,
            RedactionProfile::PreviewEnabled,
            starts_at - Duration::minutes(1),
        )
        .unwrap();
        assert_eq!(loaded.account.quota_points.len(), 1);
        assert_eq!(loaded.source.buckets.len(), 1);
        assert_eq!(loaded.source.buckets[0].token_usage.input_tokens, 42);
        assert_eq!(loaded.source.weekly_local_points, vec![weekly, next_cycle]);
        assert_eq!(
            loaded.source.buckets[0].project_groups[0].title.as_deref(),
            Some("private title")
        );

        let (ownership, lease, migrating) = migrating_ownership(&target, options.redaction_profile);
        let activation = activate_local_v2_history(
            &mut legacy,
            &target,
            &ownership,
            &lease,
            &migrating,
            &options,
        )
        .unwrap();
        let authority = ownership
            .authorize_v2_write(&lease, activation.ownership())
            .unwrap();
        let writer = target.writer(&authority).unwrap();
        let live = writer
            .record_local_observation(
                &identity,
                options.source_label,
                options.redaction_profile,
                &HistoryObservation {
                    observed_at: starts_at + Duration::minutes(30),
                    half_hour_buckets: vec![bucket(starts_at, 84)],
                    ..HistoryObservation::default()
                },
                crate::source_history::LocalObservationMode::Incremental,
            )
            .unwrap();
        assert_eq!(live.revision, first.attempt + 1);
        let overwritten = target
            .load_source_since(
                identity.node_id(),
                options.redaction_profile,
                options.window_starts_at,
            )
            .unwrap();
        assert_eq!(overwritten.buckets[0].token_usage.input_tokens, 84);
    }

    #[test]
    fn complete_marker_retries_a_new_v1_tail_before_activation() {
        let directory = tempdir().unwrap();
        let (mut legacy, target, _, _) = preview_stores(directory.path());
        let first_start = at(29, 10, 0);
        record_legacy_sample(&mut legacy, first_start, 10);
        let identity = identity(directory.path());
        let options = LocalV1MigrationOptions {
            source_identity: &identity,
            redaction_profile: RedactionProfile::PreviewEnabled,
            source_label: "local machine",
            expected_ownership_epoch: MIGRATION_EPOCH,
            window_starts_at: first_start - Duration::hours(1),
            completed_at: at(29, 11, 0),
        };
        let report = run_migration(&mut legacy, &target, &options).unwrap();
        assert!(report.v1_writer_cutover_required);
        let first_state = read_state(&migration_state_path(
            &target,
            RedactionProfile::PreviewEnabled,
        ))
        .unwrap();

        let v1_only_start = at(29, 10, 15);
        record_legacy_sample(&mut legacy, v1_only_start, 20);
        let legacy_loaded = legacy.load_since(options.window_starts_at);
        assert_eq!(legacy_loaded.half_hour_buckets.len(), 2);
        let migrated = load_migrated(
            &target,
            &identity,
            RedactionProfile::PreviewEnabled,
            options.window_starts_at,
        )
        .unwrap();
        assert_eq!(migrated.source.buckets.len(), 1);
        assert_eq!(migrated.source.buckets[0].starts_at, first_start);

        let recovered = run_migration(&mut legacy, &target, &options).unwrap();
        assert_eq!(recovered.outcome, LocalV1MigrationOutcome::Imported);
        assert_eq!(recovered.attempt, report.attempt + 1);
        assert_eq!(recovered.buckets, 2);
        let recovered_state = read_state(&migration_state_path(
            &target,
            RedactionProfile::PreviewEnabled,
        ))
        .unwrap();
        assert_eq!(recovered_state.status, MigrationStatus::Complete);
        assert_ne!(
            recovered_state.frozen_v1_digest,
            first_state.frozen_v1_digest
        );
        let migrated = load_migrated(
            &target,
            &identity,
            RedactionProfile::PreviewEnabled,
            options.window_starts_at,
        )
        .unwrap();
        assert_eq!(migrated.source.buckets.len(), 2);
        assert!(
            migrated
                .source
                .buckets
                .iter()
                .any(|bucket| bucket.starts_at == v1_only_start)
        );

        let (ownership, lease, migrating) = migrating_ownership(&target, options.redaction_profile);
        let activation = activate_local_v2_history(
            &mut legacy,
            &target,
            &ownership,
            &lease,
            &migrating,
            &options,
        )
        .unwrap();
        assert_eq!(
            activation.ownership().state(),
            HistoryOwnershipState::V2Active
        );
    }

    #[test]
    fn complete_marker_fails_closed_on_v2_corruption_when_v1_is_unchanged() {
        let directory = tempdir().unwrap();
        let (mut legacy, target, _, _) = preview_stores(directory.path());
        let starts_at = at(29, 11, 0);
        record_legacy_sample(&mut legacy, starts_at, 10);
        let identity = identity(directory.path());
        let options = LocalV1MigrationOptions {
            source_identity: &identity,
            redaction_profile: RedactionProfile::PreviewEnabled,
            source_label: "local machine",
            expected_ownership_epoch: MIGRATION_EPOCH,
            window_starts_at: starts_at - Duration::hours(1),
            completed_at: at(29, 12, 0),
        };
        let report = run_migration(&mut legacy, &target, &options).unwrap();
        assert_eq!(report.attempt, 1);

        target
            .record_source_bucket_changes(
                identity.node_id(),
                options.redaction_profile,
                &[SourceBucketRecord::upsert(2, bucket(starts_at, 999)).unwrap()],
            )
            .unwrap();
        for _ in 0..2 {
            let error = run_migration(&mut legacy, &target, &options).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
            assert!(error.to_string().contains("does not exactly match"));
            let state = read_state(&migration_state_path(
                &target,
                RedactionProfile::PreviewEnabled,
            ))
            .unwrap();
            assert_eq!(state.status, MigrationStatus::Complete);
            assert_eq!(state.attempt, 1);
        }
    }

    #[test]
    fn running_state_retries_with_a_higher_revision_and_repairs_partial_data() {
        let directory = tempdir().unwrap();
        let (mut legacy, target, _, _) = preview_stores(directory.path());
        let starts_at = at(29, 12, 0);
        record_legacy_sample(&mut legacy, starts_at, 99);
        let identity = identity(directory.path());
        let options = LocalV1MigrationOptions {
            source_identity: &identity,
            redaction_profile: RedactionProfile::PreviewEnabled,
            source_label: "local machine",
            expected_ownership_epoch: MIGRATION_EPOCH,
            window_starts_at: starts_at - Duration::hours(1),
            completed_at: at(29, 13, 0),
        };
        let imports = imports_directory(&target);
        create_private_directory(&imports).unwrap();
        let state = MigrationState::first_attempt(
            target.profile_id().clone(),
            legacy.namespace().to_string(),
            &options,
        );
        write_state(
            &migration_state_path(&target, options.redaction_profile),
            &state,
        )
        .unwrap();
        let metadata = SourceMetadata::new(
            identity.node_id().clone(),
            SourceKind::Local,
            "local machine",
        )
        .unwrap();
        target.save_source_metadata(&metadata).unwrap();
        target
            .record_account_points(&[quota_point(starts_at + Duration::minutes(5))])
            .unwrap();
        target
            .record_source_bucket_changes(
                identity.node_id(),
                RedactionProfile::PreviewEnabled,
                &[SourceBucketRecord::upsert(1, bucket(starts_at, 1)).unwrap()],
            )
            .unwrap();

        let report = run_migration(&mut legacy, &target, &options).unwrap();
        assert_eq!(report.outcome, LocalV1MigrationOutcome::Imported);
        assert_eq!(report.attempt, 2);
        let loaded = target
            .load_source_since(
                identity.node_id(),
                RedactionProfile::PreviewEnabled,
                starts_at - Duration::minutes(1),
            )
            .unwrap();
        assert_eq!(loaded.buckets.len(), 1);
        assert_eq!(loaded.buckets[0].token_usage.input_tokens, 99);

        let raw_state = read_state(&migration_state_path(
            &target,
            RedactionProfile::PreviewEnabled,
        ))
        .unwrap();
        assert_eq!(raw_state.status, MigrationStatus::Complete);
        assert_eq!(raw_state.attempt, 2);

        let (ownership, lease, migrating) = migrating_ownership(&target, options.redaction_profile);
        let activation = activate_local_v2_history(
            &mut legacy,
            &target,
            &ownership,
            &lease,
            &migrating,
            &options,
        )
        .unwrap();
        let authority = ownership
            .authorize_v2_write(&lease, activation.ownership())
            .unwrap();
        let writer = target.writer(&authority).unwrap();
        let live = writer
            .record_local_observation(
                &identity,
                options.source_label,
                options.redaction_profile,
                &HistoryObservation {
                    observed_at: starts_at + Duration::minutes(30),
                    half_hour_buckets: vec![bucket(starts_at, 123)],
                    ..HistoryObservation::default()
                },
                crate::source_history::LocalObservationMode::Incremental,
            )
            .unwrap();
        assert_eq!(live.revision, report.attempt + 1);
        let overwritten = target
            .load_source_since(
                identity.node_id(),
                options.redaction_profile,
                options.window_starts_at,
            )
            .unwrap();
        assert_eq!(overwritten.buckets[0].token_usage.input_tokens, 123);
    }

    #[test]
    fn retry_tombstones_rows_removed_from_the_retained_v1_snapshot() {
        let directory = tempdir().unwrap();
        let (mut legacy, target, _, _) = preview_stores(directory.path());
        let current_start = at(29, 12, 0);
        let stale_start = at(29, 10, 0);
        record_legacy_sample(&mut legacy, current_start, 99);
        let identity = identity(directory.path());
        let options = LocalV1MigrationOptions {
            source_identity: &identity,
            redaction_profile: RedactionProfile::PreviewEnabled,
            source_label: "local machine",
            expected_ownership_epoch: MIGRATION_EPOCH,
            window_starts_at: at(29, 9, 0),
            completed_at: at(29, 13, 0),
        };
        seed_running_attempt(&legacy, &target, &options, 1);
        target
            .record_source_bucket_changes(
                identity.node_id(),
                options.redaction_profile,
                &[SourceBucketRecord::upsert(1, bucket(stale_start, 7)).unwrap()],
            )
            .unwrap();
        let stale_weekly = weekly_point(stale_start + Duration::minutes(30));
        target
            .record_source_weekly_changes(
                identity.node_id(),
                options.redaction_profile,
                &[SourceWeeklyRecord::upsert(1, stale_weekly.clone()).unwrap()],
            )
            .unwrap();

        let report = run_migration(&mut legacy, &target, &options).unwrap();
        assert_eq!(report.attempt, 2);
        let live = target
            .load_source_since(
                identity.node_id(),
                options.redaction_profile,
                options.window_starts_at,
            )
            .unwrap();
        assert_eq!(live.buckets.len(), 1);
        assert_eq!(live.buckets[0].starts_at, current_start);
        assert!(live.weekly_local_points.is_empty());

        let records = target
            .load_source_records_since(
                identity.node_id(),
                options.redaction_profile,
                options.window_starts_at,
            )
            .unwrap();
        let stale_bucket_record = records
            .records
            .iter()
            .find(|record| record.starts_at() == stale_start)
            .unwrap();
        assert_eq!(stale_bucket_record.revision(), 2);
        assert!(matches!(
            stale_bucket_record.change(),
            SourceBucketChange::Tombstone
        ));
        let stale_weekly_record = records
            .weekly_records
            .iter()
            .find(|record| {
                record.observed_at() == stale_weekly.observed_at
                    && record.resets_at() == stale_weekly.resets_at
            })
            .unwrap();
        assert_eq!(stale_weekly_record.revision(), 2);
        assert!(matches!(
            stale_weekly_record.change(),
            SourceWeeklyChange::Tombstone
        ));
    }

    #[test]
    fn retry_after_crash_finishes_partially_published_retention_tombstones() {
        let directory = tempdir().unwrap();
        let (mut legacy, target, _, _) = preview_stores(directory.path());
        let current_start = at(29, 15, 0);
        let stale_start = at(29, 13, 0);
        record_legacy_sample(&mut legacy, current_start, 55);
        let identity = identity(directory.path());
        let options = LocalV1MigrationOptions {
            source_identity: &identity,
            redaction_profile: RedactionProfile::PreviewEnabled,
            source_label: "local machine",
            expected_ownership_epoch: MIGRATION_EPOCH,
            window_starts_at: at(29, 12, 0),
            completed_at: at(29, 16, 0),
        };
        seed_running_attempt(&legacy, &target, &options, 2);
        target
            .record_source_bucket_changes(
                identity.node_id(),
                options.redaction_profile,
                &[SourceBucketRecord::upsert(1, bucket(stale_start, 8)).unwrap()],
            )
            .unwrap();
        let stale_weekly = weekly_point(stale_start + Duration::minutes(30));
        target
            .record_source_weekly_changes(
                identity.node_id(),
                options.redaction_profile,
                &[SourceWeeklyRecord::upsert(1, stale_weekly.clone()).unwrap()],
            )
            .unwrap();
        // Simulate a crash in attempt 2 after bucket reconciliation but before
        // weekly reconciliation and the durable complete marker.
        target
            .record_source_bucket_changes(
                identity.node_id(),
                options.redaction_profile,
                &[
                    SourceBucketRecord::tombstone(stale_start, 2).unwrap(),
                    SourceBucketRecord::upsert(2, bucket(current_start, 55)).unwrap(),
                ],
            )
            .unwrap();

        let report = run_migration(&mut legacy, &target, &options).unwrap();
        assert_eq!(report.attempt, 3);
        let live = target
            .load_source_since(
                identity.node_id(),
                options.redaction_profile,
                options.window_starts_at,
            )
            .unwrap();
        assert_eq!(live.buckets, vec![bucket(current_start, 55)]);
        assert!(live.weekly_local_points.is_empty());

        let records = target
            .load_source_records_since(
                identity.node_id(),
                options.redaction_profile,
                options.window_starts_at,
            )
            .unwrap();
        let stale_bucket_record = records
            .records
            .iter()
            .find(|record| record.starts_at() == stale_start)
            .unwrap();
        assert_eq!(stale_bucket_record.revision(), 2);
        assert!(matches!(
            stale_bucket_record.change(),
            SourceBucketChange::Tombstone
        ));
        let current_bucket_record = records
            .records
            .iter()
            .find(|record| record.starts_at() == current_start)
            .unwrap();
        assert_eq!(current_bucket_record.revision(), 3);
        let stale_weekly_record = records
            .weekly_records
            .iter()
            .find(|record| {
                record.observed_at() == stale_weekly.observed_at
                    && record.resets_at() == stale_weekly.resets_at
            })
            .unwrap();
        assert_eq!(stale_weekly_record.revision(), 3);
        assert!(matches!(
            stale_weekly_record.change(),
            SourceWeeklyChange::Tombstone
        ));
    }

    #[test]
    fn migration_refuses_complete_when_account_point_is_not_exact() {
        let directory = tempdir().unwrap();
        let (mut legacy, target, _, _) = preview_stores(directory.path());
        let starts_at = at(29, 12, 0);
        record_legacy_sample(&mut legacy, starts_at, 99);
        let identity = identity(directory.path());
        let options = LocalV1MigrationOptions {
            source_identity: &identity,
            redaction_profile: RedactionProfile::PreviewEnabled,
            source_label: "local machine",
            expected_ownership_epoch: MIGRATION_EPOCH,
            window_starts_at: starts_at - Duration::hours(1),
            completed_at: at(29, 13, 0),
        };
        let imports = imports_directory(&target);
        create_private_directory(&imports).unwrap();
        let state = MigrationState::first_attempt(
            target.profile_id().clone(),
            legacy.namespace().to_string(),
            &options,
        );
        write_state(
            &migration_state_path(&target, options.redaction_profile),
            &state,
        )
        .unwrap();
        let mut conflicting = quota_point(starts_at + Duration::minutes(5));
        conflicting.used_percent = 50.0;
        conflicting.remaining_percent = 50.0;
        target.record_account_points(&[conflicting]).unwrap();

        let error = run_migration(&mut legacy, &target, &options).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        let state = read_state(&migration_state_path(
            &target,
            RedactionProfile::PreviewEnabled,
        ))
        .unwrap();
        assert_eq!(state.status, MigrationStatus::Running);
        assert_eq!(state.attempt, 2);
    }

    #[test]
    fn migration_refuses_a_source_revision_not_owned_by_an_earlier_attempt() {
        let directory = tempdir().unwrap();
        let (mut legacy, target, _, _) = preview_stores(directory.path());
        let starts_at = at(29, 12, 0);
        record_legacy_sample(&mut legacy, starts_at, 99);
        let identity = identity(directory.path());
        let options = LocalV1MigrationOptions {
            source_identity: &identity,
            redaction_profile: RedactionProfile::PreviewEnabled,
            source_label: "local machine",
            expected_ownership_epoch: MIGRATION_EPOCH,
            window_starts_at: starts_at - Duration::hours(1),
            completed_at: at(29, 13, 0),
        };
        let imports = imports_directory(&target);
        create_private_directory(&imports).unwrap();
        let state = MigrationState::first_attempt(
            target.profile_id().clone(),
            legacy.namespace().to_string(),
            &options,
        );
        write_state(
            &migration_state_path(&target, options.redaction_profile),
            &state,
        )
        .unwrap();
        let metadata = SourceMetadata::new(
            identity.node_id().clone(),
            SourceKind::Local,
            "local machine",
        )
        .unwrap();
        target.save_source_metadata(&metadata).unwrap();
        target
            .record_source_bucket_changes(
                identity.node_id(),
                RedactionProfile::PreviewEnabled,
                &[
                    SourceBucketRecord::upsert(99, bucket(starts_at + Duration::minutes(15), 1))
                        .unwrap(),
                ],
            )
            .unwrap();

        let error = run_migration(&mut legacy, &target, &options).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("not owned"));
        let state = read_state(&migration_state_path(
            &target,
            RedactionProfile::PreviewEnabled,
        ))
        .unwrap();
        assert_eq!(state.status, MigrationStatus::Running);
        assert_eq!(state.attempt, 1);
    }

    #[test]
    fn redacted_v1_content_stays_in_the_redacted_v2_namespace() {
        let directory = tempdir().unwrap();
        let state_root = directory.path().join("state");
        let codex_home = directory.path().join("codex-home");
        let mut legacy = HistoryStore::new_with_redaction(
            state_root.join(LEGACY_HISTORY_DIRECTORY),
            &codex_home,
            true,
        );
        let profile_id = legacy
            .namespace()
            .strip_suffix("-redacted")
            .unwrap()
            .parse::<HistoryProfileId>()
            .unwrap();
        let target = SourceHistoryStore::new(state_root, profile_id);
        let starts_at = at(29, 14, 0);
        record_legacy_sample(&mut legacy, starts_at, 10);
        let identity = identity(directory.path());
        let options = LocalV1MigrationOptions {
            source_identity: &identity,
            redaction_profile: RedactionProfile::Redacted,
            source_label: "local machine",
            expected_ownership_epoch: MIGRATION_EPOCH,
            window_starts_at: starts_at - Duration::hours(1),
            completed_at: at(29, 15, 0),
        };

        run_migration(&mut legacy, &target, &options).unwrap();
        let loaded = target
            .load_source_since(
                identity.node_id(),
                RedactionProfile::Redacted,
                starts_at - Duration::minutes(1),
            )
            .unwrap();
        let group = &loaded.buckets[0].project_groups[0];
        assert_eq!(group.title.as_deref(), Some("[redacted]"));
        assert_eq!(group.message_preview.as_deref(), Some("[redacted]"));
        assert!(
            !target
                .source_buckets_directory(identity.node_id(), RedactionProfile::PreviewEnabled)
                .exists()
        );
    }

    #[test]
    fn malformed_state_fails_closed_before_source_or_account_publication() {
        let directory = tempdir().unwrap();
        let (mut legacy, target, _, _) = preview_stores(directory.path());
        let starts_at = at(29, 16, 0);
        record_legacy_sample(&mut legacy, starts_at, 7);
        let identity = identity(directory.path());
        let options = LocalV1MigrationOptions {
            source_identity: &identity,
            redaction_profile: RedactionProfile::PreviewEnabled,
            source_label: "local machine",
            expected_ownership_epoch: MIGRATION_EPOCH,
            window_starts_at: starts_at - Duration::hours(1),
            completed_at: at(29, 17, 0),
        };
        let imports = imports_directory(&target);
        create_private_directory(&imports).unwrap();
        let state_path = migration_state_path(&target, options.redaction_profile);
        let mut file_options = OpenOptions::new();
        file_options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            file_options.mode(0o600);
        }
        file_options
            .open(&state_path)
            .unwrap()
            .write_all(b"{not-json")
            .unwrap();

        let error = run_migration(&mut legacy, &target, &options).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(!target.account_directory().exists());
        assert!(!target.source_directory(identity.node_id()).exists());
    }

    #[test]
    fn existing_source_buckets_without_state_are_never_claimed_as_an_import() {
        let directory = tempdir().unwrap();
        let (mut legacy, target, _, _) = preview_stores(directory.path());
        let starts_at = at(29, 17, 0);
        record_legacy_sample(&mut legacy, starts_at, 70);
        let identity = identity(directory.path());
        target
            .save_source_metadata(
                &SourceMetadata::new(
                    identity.node_id().clone(),
                    SourceKind::Local,
                    "local machine",
                )
                .unwrap(),
            )
            .unwrap();
        target
            .record_source_bucket_changes(
                identity.node_id(),
                RedactionProfile::PreviewEnabled,
                &[SourceBucketRecord::upsert(99, bucket(starts_at, 1)).unwrap()],
            )
            .unwrap();
        let options = LocalV1MigrationOptions {
            source_identity: &identity,
            redaction_profile: RedactionProfile::PreviewEnabled,
            source_label: "local machine",
            expected_ownership_epoch: MIGRATION_EPOCH,
            window_starts_at: starts_at - Duration::hours(1),
            completed_at: at(29, 18, 0),
        };

        let error = run_migration(&mut legacy, &target, &options).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert!(!migration_state_path(&target, options.redaction_profile).exists());
        let loaded = target
            .load_source_since(
                identity.node_id(),
                RedactionProfile::PreviewEnabled,
                starts_at - Duration::minutes(1),
            )
            .unwrap();
        assert_eq!(loaded.buckets[0].token_usage.input_tokens, 1);
    }

    #[test]
    fn existing_session_evidence_without_state_is_never_claimed_as_an_import() {
        for namespace in ["digests", "facts", "fact-manifests", "fact-staging"] {
            let directory = tempdir().unwrap();
            let (_, target, _, _) = preview_stores(directory.path());
            let identity = identity(directory.path());
            let path = match namespace {
                "digests" => target
                    .source_digests_directory(identity.node_id(), RedactionProfile::PreviewEnabled),
                "facts" => target
                    .source_facts_directory(identity.node_id(), RedactionProfile::PreviewEnabled),
                "fact-manifests" => target.source_fact_manifests_directory(
                    identity.node_id(),
                    RedactionProfile::PreviewEnabled,
                ),
                "fact-staging" => target.source_fact_staging_directory(
                    identity.node_id(),
                    RedactionProfile::PreviewEnabled,
                ),
                _ => unreachable!(),
            };
            target.prepare_private_directory(&path).unwrap();

            let error = ensure_uninitialized_source_namespace(
                &target,
                identity.node_id(),
                RedactionProfile::PreviewEnabled,
            )
            .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
            assert!(
                error
                    .to_string()
                    .contains("without a matching migration state"),
                "namespace={namespace}"
            );
        }
    }

    #[test]
    fn identity_change_during_a_running_attempt_fails_instead_of_double_importing() {
        let directory = tempdir().unwrap();
        let (mut legacy, target, _, _) = preview_stores(directory.path());
        let starts_at = at(29, 18, 0);
        record_legacy_sample(&mut legacy, starts_at, 7);
        let identity_store =
            SourceIdentityStore::at_path(directory.path().join("identity/source-identity.json"));
        let first_identity = identity_store.load_or_create().unwrap();
        let first_options = LocalV1MigrationOptions {
            source_identity: &first_identity,
            redaction_profile: RedactionProfile::PreviewEnabled,
            source_label: "local machine",
            expected_ownership_epoch: MIGRATION_EPOCH,
            window_starts_at: starts_at - Duration::hours(1),
            completed_at: at(29, 19, 0),
        };
        let imports = imports_directory(&target);
        create_private_directory(&imports).unwrap();
        write_state(
            &migration_state_path(&target, first_options.redaction_profile),
            &MigrationState::first_attempt(
                target.profile_id().clone(),
                legacy.namespace().to_string(),
                &first_options,
            ),
        )
        .unwrap();

        let rotated = identity_store.rotate().unwrap();
        let rotated_options = LocalV1MigrationOptions {
            source_identity: &rotated,
            ..first_options
        };
        let error = run_migration(&mut legacy, &target, &rotated_options).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(!target.source_directory(rotated.node_id()).exists());
        assert!(!target.source_directory(first_identity.node_id()).exists());
    }

    #[test]
    fn recovery_classifies_missing_running_and_complete_under_the_same_writer_lease() {
        let directory = tempdir().unwrap();
        let (mut legacy, target, _, _) = preview_stores(directory.path());
        let starts_at = at(29, 19, 0);
        record_legacy_sample(&mut legacy, starts_at, 17);
        let identity = identity(directory.path());
        let options = LocalV1MigrationOptions {
            source_identity: &identity,
            redaction_profile: RedactionProfile::PreviewEnabled,
            source_label: "local machine",
            expected_ownership_epoch: MIGRATION_EPOCH,
            window_starts_at: starts_at - Duration::hours(1),
            completed_at: at(29, 20, 0),
        };
        let (ownership, lease, manifest) = migrating_ownership(&target, options.redaction_profile);

        assert_eq!(
            inspect_local_v1_migration_recovery(
                &legacy, &target, &ownership, &lease, &manifest, &options,
            )
            .unwrap(),
            LocalV1MigrationRecoveryStatus::Missing
        );

        let imports = imports_directory(&target);
        create_private_directory(&imports).unwrap();
        open_lock_file(&imports).unwrap();
        write_state(
            &migration_state_path(&target, options.redaction_profile),
            &MigrationState::first_attempt(
                target.profile_id().clone(),
                legacy.namespace().to_string(),
                &options,
            ),
        )
        .unwrap();
        let LocalV1MigrationRecoveryStatus::Running(running) = inspect_local_v1_migration_recovery(
            &legacy, &target, &ownership, &lease, &manifest, &options,
        )
        .unwrap() else {
            panic!("expected a running recovery marker");
        };
        assert_eq!(running.ownership_epoch(), MIGRATION_EPOCH);
        assert_eq!(running.attempt(), 1);
        assert!(running.completed_at().is_none());

        migrate_local_v1_history(
            &mut legacy,
            &target,
            &ownership,
            &lease,
            &manifest,
            &options,
        )
        .unwrap();
        let LocalV1MigrationRecoveryStatus::Complete(complete) =
            inspect_local_v1_migration_recovery(
                &legacy, &target, &ownership, &lease, &manifest, &options,
            )
            .unwrap()
        else {
            panic!("expected a complete recovery marker");
        };
        assert_eq!(complete.ownership_epoch(), MIGRATION_EPOCH);
        assert_eq!(complete.attempt(), 2);
        assert_eq!(complete.quota_points(), Some(1));
        assert_eq!(complete.buckets(), Some(1));
        assert_eq!(complete.weekly_local_points(), Some(0));
    }

    #[test]
    fn recovery_rejects_corrupt_or_epoch_mismatched_markers() {
        let directory = tempdir().unwrap();
        let (legacy, target, _, _) = preview_stores(directory.path());
        let identity = identity(directory.path());
        let options = LocalV1MigrationOptions {
            source_identity: &identity,
            redaction_profile: RedactionProfile::PreviewEnabled,
            source_label: "local machine",
            expected_ownership_epoch: MIGRATION_EPOCH,
            window_starts_at: at(29, 9, 0),
            completed_at: at(29, 20, 0),
        };
        let (ownership, lease, manifest) = migrating_ownership(&target, options.redaction_profile);
        let imports = imports_directory(&target);
        create_private_directory(&imports).unwrap();
        open_lock_file(&imports).unwrap();
        let state_path = migration_state_path(&target, options.redaction_profile);
        write_private_test_file(&state_path, b"{not-json");
        assert_eq!(
            inspect_local_v1_migration_recovery(
                &legacy, &target, &ownership, &lease, &manifest, &options,
            )
            .unwrap_err()
            .kind(),
            io::ErrorKind::InvalidData
        );

        let mut wrong_epoch = MigrationState::first_attempt(
            target.profile_id().clone(),
            legacy.namespace().into(),
            &options,
        );
        wrong_epoch.expected_ownership_epoch += 1;
        write_state(&state_path, &wrong_epoch).unwrap();
        let error = inspect_local_v1_migration_recovery(
            &legacy, &target, &ownership, &lease, &manifest, &options,
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("requested source and namespace"));
    }

    #[test]
    fn activation_evidence_requires_current_lease_epoch_and_frozen_v1_match() {
        let directory = tempdir().unwrap();
        let (mut legacy, target, _, _) = preview_stores(directory.path());
        let starts_at = at(29, 20, 0);
        record_legacy_sample(&mut legacy, starts_at, 23);
        let identity = identity(directory.path());
        let options = LocalV1MigrationOptions {
            source_identity: &identity,
            redaction_profile: RedactionProfile::PreviewEnabled,
            source_label: "local machine",
            expected_ownership_epoch: MIGRATION_EPOCH,
            window_starts_at: starts_at - Duration::hours(1),
            completed_at: at(29, 21, 0),
        };
        let (ownership, lease, manifest) = migrating_ownership(&target, options.redaction_profile);
        migrate_local_v1_history(
            &mut legacy,
            &target,
            &ownership,
            &lease,
            &manifest,
            &options,
        )
        .unwrap();

        let evidence = verify_local_v1_migration_for_activation(
            &mut legacy,
            &target,
            &ownership,
            &lease,
            &manifest,
            &options,
        )
        .unwrap();
        assert_eq!(evidence.ownership_epoch(), MIGRATION_EPOCH);
        assert_eq!(evidence.attempt(), 1);
        assert_eq!(evidence.completed_at(), options.completed_at);

        record_legacy_sample(&mut legacy, starts_at + Duration::minutes(15), 99);
        let error = verify_local_v1_migration_for_activation(
            &mut legacy,
            &target,
            &ownership,
            &lease,
            &manifest,
            &options,
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("frozen v1 snapshot"));
    }

    #[test]
    fn verified_activation_transitions_the_same_epoch_to_v2_active() {
        let directory = tempdir().unwrap();
        let (mut legacy, target, _, _) = preview_stores(directory.path());
        let starts_at = at(29, 20, 30);
        record_legacy_sample(&mut legacy, starts_at, 29);
        let identity = identity(directory.path());
        let options = LocalV1MigrationOptions {
            source_identity: &identity,
            redaction_profile: RedactionProfile::PreviewEnabled,
            source_label: "local machine",
            expected_ownership_epoch: MIGRATION_EPOCH,
            window_starts_at: starts_at - Duration::hours(1),
            completed_at: at(29, 21, 30),
        };
        let (ownership, lease, migrating) = migrating_ownership(&target, options.redaction_profile);
        migrate_local_v1_history(
            &mut legacy,
            &target,
            &ownership,
            &lease,
            &migrating,
            &options,
        )
        .unwrap();

        let activation = activate_local_v2_history(
            &mut legacy,
            &target,
            &ownership,
            &lease,
            &migrating,
            &options,
        )
        .unwrap();
        assert_eq!(activation.evidence().ownership_epoch(), MIGRATION_EPOCH);
        assert_eq!(activation.ownership().epoch(), MIGRATION_EPOCH);
        assert_eq!(
            activation.ownership().state(),
            HistoryOwnershipState::V2Active
        );
        assert_eq!(
            ownership.load_manifest().unwrap(),
            OwnershipManifestStatus::Initialized(activation.ownership().clone())
        );
        let loaded = load_migrated_local_history_since(
            &target,
            &ownership,
            activation.ownership(),
            &identity,
            options.redaction_profile,
            options.window_starts_at,
        )
        .unwrap();
        assert_eq!(loaded.source.buckets.len(), 1);

        let error = activate_local_v2_history(
            &mut legacy,
            &target,
            &ownership,
            &lease,
            &migrating,
            &options,
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn complete_marker_and_query_reject_a_different_ownership_epoch() {
        let directory = tempdir().unwrap();
        let (mut legacy, target, _, _) = preview_stores(directory.path());
        let starts_at = at(29, 21, 0);
        record_legacy_sample(&mut legacy, starts_at, 31);
        let identity = identity(directory.path());
        let options = LocalV1MigrationOptions {
            source_identity: &identity,
            redaction_profile: RedactionProfile::PreviewEnabled,
            source_label: "local machine",
            expected_ownership_epoch: MIGRATION_EPOCH,
            window_starts_at: starts_at - Duration::hours(1),
            completed_at: at(29, 22, 0),
        };
        run_migration(&mut legacy, &target, &options).unwrap();
        let state_path = migration_state_path(&target, options.redaction_profile);
        let mut state = read_state(&state_path).unwrap();
        state.expected_ownership_epoch += 1;
        write_state(&state_path, &state).unwrap();

        assert_eq!(
            run_migration(&mut legacy, &target, &options)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(
            load_migrated(
                &target,
                &identity,
                options.redaction_profile,
                options.window_starts_at,
            )
            .unwrap_err()
            .kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn incomplete_state_is_not_queryable() {
        let directory = tempdir().unwrap();
        let (legacy, target, _, _) = preview_stores(directory.path());
        let identity = identity(directory.path());
        let options = LocalV1MigrationOptions {
            source_identity: &identity,
            redaction_profile: RedactionProfile::PreviewEnabled,
            source_label: "local machine",
            expected_ownership_epoch: MIGRATION_EPOCH,
            window_starts_at: at(29, 9, 0),
            completed_at: at(29, 20, 0),
        };
        let _ownership_guard = migrating_ownership(&target, options.redaction_profile);
        let imports = imports_directory(&target);
        create_private_directory(&imports).unwrap();
        open_lock_file(&imports).unwrap();
        write_state(
            &migration_state_path(&target, options.redaction_profile),
            &MigrationState::first_attempt(
                target.profile_id().clone(),
                legacy.namespace().to_string(),
                &options,
            ),
        )
        .unwrap();

        let error = load_migrated(
            &target,
            &identity,
            RedactionProfile::PreviewEnabled,
            at(29, 9, 0),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
    }

    #[test]
    fn windows_reparse_policy_and_migration_lock_sharing_fail_closed() {
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        const FILE_SHARE_DELETE: u32 = 0x4;

        assert!(windows_attributes_are_reparse(
            FILE_ATTRIBUTE_REPARSE_POINT,
            FILE_ATTRIBUTE_REPARSE_POINT
        ));
        assert!(!windows_attributes_are_reparse(
            0,
            FILE_ATTRIBUTE_REPARSE_POINT
        ));
        assert_eq!(stable_lock_share_mode_for_test() & FILE_SHARE_DELETE, 0);
    }

    #[cfg(unix)]
    #[test]
    fn migration_imports_reject_symlinked_layout_ancestor() {
        use std::os::unix::fs::symlink;

        let directory = tempdir().unwrap();
        let state_root = directory.path().join("state");
        create_private_directory(&state_root).unwrap();
        let outside = directory.path().join("outside");
        create_private_directory(&outside).unwrap();
        symlink(&outside, state_root.join("history-v2")).unwrap();
        let target = SourceHistoryStore::new(
            state_root,
            "0123456789abcdef".parse::<HistoryProfileId>().unwrap(),
        );

        let error = target
            .prepare_private_directory(&imports_directory(&target))
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }
}
