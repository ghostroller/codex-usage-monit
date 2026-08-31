use std::collections::BTreeSet;
use std::io;
use std::num::NonZeroU64;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::{
    HistoryProfileId, RedactionProfile, SourceHistoryStore, SourceHistoryWriter, SourceKind,
    SourceMetadata, encode_pretty_bounded, invalid_data, lock_exclusive, open_lock_file,
    read_optional_json_file, write_private_atomically,
};
use crate::remote_protocol::{
    ProtocolRevisions, RemoteLiveSnapshot, RemoteLiveState, RemoteProjectDescriptor,
    SourceGeneration,
};
use crate::source_identity::NodeId;

const REMOTE_LIVE_FORMAT_VERSION: u32 = 1;
const REMOTE_LIVE_FILE: &str = "remote-live.json";
const REMOTE_LIVE_MAX_BYTES: u64 = 8 * 1024 * 1024;
const SOURCE_LOCK_FILE: &str = "source.lock";
const MAX_QUALITY_REASONS: usize = 128;
const MAX_QUALITY_REASON_BYTES: usize = 128;

/// One center-owned, source-bound live replacement suitable for cross-process
/// readers. The SSH source metadata is returned alongside the wire snapshot so
/// presentation can preserve a stable origin label without persisting remote
/// paths.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceRemoteLiveSnapshot {
    pub source: SourceMetadata,
    pub source_generation: SourceGeneration,
    pub revisions: ProtocolRevisions,
    pub redaction_profile: RedactionProfile,
    pub live_revision: NonZeroU64,
    pub snapshot: RemoteLiveSnapshot,
    pub project_descriptors: Vec<RemoteProjectDescriptor>,
    pub remote_observed_at: DateTime<Utc>,
    pub received_at: DateTime<Utc>,
    pub range_complete: bool,
    pub partial_reasons: Vec<String>,
    pub warning_codes: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredRemoteLiveSnapshot {
    format_version: u32,
    profile_id: HistoryProfileId,
    source_generation: SourceGeneration,
    revisions: ProtocolRevisions,
    redaction_profile: RedactionProfile,
    live_revision: NonZeroU64,
    snapshot: RemoteLiveSnapshot,
    project_descriptors: Vec<RemoteProjectDescriptor>,
    remote_observed_at: DateTime<Utc>,
    received_at: DateTime<Utc>,
    range_complete: bool,
    partial_reasons: Vec<String>,
    warning_codes: Vec<String>,
}

impl StoredRemoteLiveSnapshot {
    fn validate(
        &self,
        profile_id: &HistoryProfileId,
        source_id: &NodeId,
        redaction_profile: RedactionProfile,
    ) -> io::Result<()> {
        if self.format_version != REMOTE_LIVE_FORMAT_VERSION
            || &self.profile_id != profile_id
            || &self.source_generation.node_id != source_id
            || self.redaction_profile != redaction_profile
        {
            return Err(invalid_data(
                "remote live state does not match its source/profile namespace",
            ));
        }
        if self.snapshot.captured_at > self.remote_observed_at {
            return Err(invalid_data("remote live state timestamps are invalid"));
        }
        self.snapshot
            .validate_for_storage(redaction_profile)
            .map_err(|error| invalid_data(format!("remote live snapshot is invalid: {error}")))?;
        validate_descriptors(&self.snapshot, &self.project_descriptors)?;
        validate_quality_reasons(&self.partial_reasons, "partial reason")?;
        validate_quality_reasons(&self.warning_codes, "warning code")?;
        if self.range_complete
            && (!self.partial_reasons.is_empty() || !self.warning_codes.is_empty())
        {
            return Err(invalid_data(
                "complete remote live coverage cannot contain partial reasons",
            ));
        }
        Ok(())
    }

    fn into_public(self, source: SourceMetadata) -> SourceRemoteLiveSnapshot {
        SourceRemoteLiveSnapshot {
            source,
            source_generation: self.source_generation,
            revisions: self.revisions,
            redaction_profile: self.redaction_profile,
            live_revision: self.live_revision,
            snapshot: self.snapshot,
            project_descriptors: self.project_descriptors,
            remote_observed_at: self.remote_observed_at,
            received_at: self.received_at,
            range_complete: self.range_complete,
            partial_reasons: self.partial_reasons,
            warning_codes: self.warning_codes,
        }
    }
}

impl SourceHistoryWriter<'_, '_, '_> {
    /// Publishes a validated page's live state before its ingest cursor is
    /// acknowledged. Replaying the same WAL page is idempotent; a revision-only
    /// page without the exact cached baseline fails closed.
    #[allow(clippy::too_many_arguments)]
    pub fn record_remote_live_state(
        &self,
        source_generation: &SourceGeneration,
        revisions: &ProtocolRevisions,
        redaction_profile: RedactionProfile,
        live: &RemoteLiveState,
        project_descriptors: &[RemoteProjectDescriptor],
        remote_observed_at: DateTime<Utc>,
        received_at: DateTime<Utc>,
        range_complete: bool,
        partial_reasons: &[String],
        warning_codes: &[String],
    ) -> io::Result<()> {
        self.validate_redaction(redaction_profile)?;
        self.fenced(|store| {
            store.record_remote_live_state_unfenced(
                source_generation,
                revisions,
                redaction_profile,
                live,
                project_descriptors,
                remote_observed_at,
                received_at,
                range_complete,
                partial_reasons,
                warning_codes,
            )
        })
    }
}

impl SourceHistoryStore {
    pub fn remote_live_revision_for_binding(
        &self,
        source_generation: &SourceGeneration,
        revisions: &ProtocolRevisions,
        redaction_profile: RedactionProfile,
    ) -> io::Result<Option<NonZeroU64>> {
        Ok(self
            .load_remote_live_state(&source_generation.node_id)?
            .filter(|state| {
                state.source_generation == *source_generation
                    && state.revisions == *revisions
                    && state.redaction_profile == redaction_profile
            })
            .map(|state| state.live_revision))
    }

    pub fn load_remote_live_state(
        &self,
        source_id: &NodeId,
    ) -> io::Result<Option<SourceRemoteLiveSnapshot>> {
        self.with_source_metadata_shared(source_id, |metadata| {
            if metadata.kind() != SourceKind::Ssh {
                return Ok(None);
            }
            let profile = metadata.aggregate_redaction_profile();
            let path = self.remote_live_path(source_id, profile);
            let Some(stored) =
                read_optional_json_file::<StoredRemoteLiveSnapshot>(&path, REMOTE_LIVE_MAX_BYTES)?
            else {
                return Ok(None);
            };
            stored.validate(&self.profile_id, source_id, profile)?;
            Ok(Some(stored.into_public(metadata.clone())))
        })
    }

    pub fn load_included_remote_live_states(&self) -> io::Result<Vec<SourceRemoteLiveSnapshot>> {
        let mut states = Vec::new();
        for metadata in self.list_source_metadata()? {
            if metadata.kind() != SourceKind::Ssh || !metadata.include_in_aggregates() {
                continue;
            }
            if let Some(state) = self.load_remote_live_state(metadata.source_id())?
                && state.source.include_in_aggregates()
            {
                states.push(state);
            }
        }
        states.sort_by(|left, right| {
            left.source
                .source_id()
                .as_str()
                .cmp(right.source.source_id().as_str())
        });
        Ok(states)
    }

    fn remote_live_path(
        &self,
        source_id: &NodeId,
        profile: RedactionProfile,
    ) -> std::path::PathBuf {
        self.source_directory(source_id)
            .join(profile.directory_name())
            .join(REMOTE_LIVE_FILE)
    }

    #[allow(clippy::too_many_arguments)]
    fn record_remote_live_state_unfenced(
        &self,
        source_generation: &SourceGeneration,
        revisions: &ProtocolRevisions,
        redaction_profile: RedactionProfile,
        live: &RemoteLiveState,
        project_descriptors: &[RemoteProjectDescriptor],
        remote_observed_at: DateTime<Utc>,
        received_at: DateTime<Utc>,
        range_complete: bool,
        partial_reasons: &[String],
        warning_codes: &[String],
    ) -> io::Result<()> {
        let source_id = &source_generation.node_id;
        let directory = self.source_directory(source_id);
        self.validate_private_path(&directory)?;
        let lock = open_lock_file(&directory, SOURCE_LOCK_FILE)?;
        lock_exclusive(&lock, &directory, SOURCE_LOCK_FILE)?;
        let metadata = super::read_source_metadata_file(
            &directory.join(super::SOURCE_METADATA_FILE),
            &self.profile_id,
            source_id,
        )?;
        if metadata.kind() != SourceKind::Ssh
            || metadata.aggregate_redaction_profile() != redaction_profile
        {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "remote live publication raced a source profile change",
            ));
        }
        let path = self.remote_live_path(source_id, redaction_profile);
        let existing =
            read_optional_json_file::<StoredRemoteLiveSnapshot>(&path, REMOTE_LIVE_MAX_BYTES)?;
        if let Some(existing) = existing.as_ref() {
            existing.validate(&self.profile_id, source_id, redaction_profile)?;
            if (existing.source_generation != *source_generation
                || existing.revisions != *revisions)
                && live.snapshot.is_none()
            {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "revision-only remote live publication has no matching binding",
                ));
            }
        }

        let existing = existing.as_ref().filter(|existing| {
            existing.source_generation == *source_generation && existing.revisions == *revisions
        });
        let (snapshot, descriptors) = match (&live.snapshot, existing) {
            (Some(snapshot), Some(existing)) if live.live_revision == existing.live_revision => {
                if snapshot != &existing.snapshot
                    || project_descriptors != existing.project_descriptors.as_slice()
                {
                    return Err(invalid_data(
                        "remote live revision was reused with different content",
                    ));
                }
                (snapshot.clone(), project_descriptors.to_vec())
            }
            (Some(snapshot), existing) => {
                if existing.is_some_and(|existing| live.live_revision < existing.live_revision) {
                    return Err(invalid_data("remote live revision regressed"));
                }
                (snapshot.clone(), project_descriptors.to_vec())
            }
            (None, Some(existing)) if live.live_revision == existing.live_revision => {
                if !project_descriptors.is_empty() {
                    return Err(invalid_data(
                        "revision-only remote live page contains descriptors",
                    ));
                }
                (
                    existing.snapshot.clone(),
                    existing.project_descriptors.clone(),
                )
            }
            (None, _) => {
                return Err(invalid_data(
                    "revision-only remote live page has no exact cached baseline",
                ));
            }
        };
        let stored = StoredRemoteLiveSnapshot {
            format_version: REMOTE_LIVE_FORMAT_VERSION,
            profile_id: self.profile_id.clone(),
            source_generation: source_generation.clone(),
            revisions: revisions.clone(),
            redaction_profile,
            live_revision: live.live_revision,
            snapshot,
            project_descriptors: descriptors,
            remote_observed_at,
            received_at,
            range_complete,
            partial_reasons: partial_reasons.to_vec(),
            warning_codes: warning_codes.to_vec(),
        };
        stored.validate(&self.profile_id, source_id, redaction_profile)?;
        self.prepare_private_directory(path.parent().expect("remote live file has a parent"))?;
        let contents = encode_pretty_bounded(&stored, REMOTE_LIVE_MAX_BYTES)?;
        write_private_atomically(&path, &contents)
    }
}

fn validate_descriptors(
    snapshot: &RemoteLiveSnapshot,
    descriptors: &[RemoteProjectDescriptor],
) -> io::Result<()> {
    let mut described = BTreeSet::new();
    for descriptor in descriptors {
        descriptor
            .validate_for_storage()
            .map_err(|error| invalid_data(format!("remote live descriptor is invalid: {error}")))?;
        if !described.insert(descriptor.observed_project_key.as_str()) {
            return Err(invalid_data("remote live descriptors contain duplicates"));
        }
    }
    let referenced = snapshot
        .tasks
        .iter()
        .filter_map(|task| task.observed_project_key.as_ref())
        .map(|key| key.as_str())
        .collect::<BTreeSet<_>>();
    if described != referenced {
        return Err(invalid_data(
            "remote live descriptors do not exactly match snapshot references",
        ));
    }
    Ok(())
}

fn validate_quality_reasons(reasons: &[String], subject: &str) -> io::Result<()> {
    if reasons.len() > MAX_QUALITY_REASONS {
        return Err(invalid_data(format!(
            "remote live {subject} count is too large"
        )));
    }
    let mut previous = None;
    for reason in reasons {
        if reason.is_empty()
            || reason.len() > MAX_QUALITY_REASON_BYTES
            || !reason
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
            || previous.is_some_and(|previous: &str| previous >= reason.as_str())
        {
            return Err(invalid_data(format!("remote live {subject} is invalid")));
        }
        previous = Some(reason);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, TimeZone};
    use tempfile::tempdir;

    fn at(hour: u32, minute: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 30, hour, minute, 0)
            .single()
            .unwrap()
    }

    fn source() -> SourceGeneration {
        SourceGeneration {
            node_id: "node-0123456789abcdef0123456789abcdef".parse().unwrap(),
            generation: NonZeroU64::new(1).unwrap(),
        }
    }

    fn full_live(revision: u64, captured_at: DateTime<Utc>) -> RemoteLiveState {
        RemoteLiveState {
            live_revision: NonZeroU64::new(revision).unwrap(),
            snapshot: Some(RemoteLiveSnapshot {
                captured_at,
                tasks: Vec::new(),
                turns: Vec::new(),
            }),
        }
    }

    fn fixture() -> (tempfile::TempDir, SourceHistoryStore, SourceGeneration) {
        let directory = tempdir().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))
                .unwrap();
        }
        let source = source();
        let store = SourceHistoryStore::new(
            directory.path().to_path_buf(),
            "0123456789abcdef".parse().unwrap(),
        );
        store
            .save_source_metadata(
                &SourceMetadata::new(source.node_id.clone(), SourceKind::Ssh, "dev-server")
                    .unwrap(),
            )
            .unwrap();
        (directory, store, source)
    }

    #[test]
    fn full_live_state_is_durable_and_revision_only_requires_the_exact_local_baseline() {
        let (_directory, store, source) = fixture();
        let revisions = crate::remote_agent::current_revisions();
        let captured_at = at(12, 0);
        store
            .record_remote_live_state_unfenced(
                &source,
                &revisions,
                RedactionProfile::Redacted,
                &full_live(1, captured_at),
                &[],
                captured_at,
                captured_at + Duration::seconds(1),
                true,
                &[],
                &[],
            )
            .unwrap();

        let loaded = store
            .load_remote_live_state(&source.node_id)
            .unwrap()
            .unwrap();
        assert_eq!(loaded.live_revision, NonZeroU64::new(1).unwrap());
        assert_eq!(loaded.snapshot.captured_at, captured_at);
        assert!(loaded.range_complete);
        assert_eq!(
            store
                .remote_live_revision_for_binding(&source, &revisions, RedactionProfile::Redacted,)
                .unwrap(),
            Some(NonZeroU64::new(1).unwrap())
        );

        let revision_only = RemoteLiveState {
            live_revision: NonZeroU64::new(1).unwrap(),
            snapshot: None,
        };
        store
            .record_remote_live_state_unfenced(
                &source,
                &revisions,
                RedactionProfile::Redacted,
                &revision_only,
                &[],
                captured_at + Duration::minutes(1),
                captured_at + Duration::minutes(1),
                true,
                &[],
                &[],
            )
            .unwrap();
        assert_eq!(
            store
                .load_remote_live_state(&source.node_id)
                .unwrap()
                .unwrap()
                .snapshot
                .captured_at,
            captured_at,
            "revision-only pages preserve the exact cached replacement"
        );

        std::fs::remove_file(store.remote_live_path(&source.node_id, RedactionProfile::Redacted))
            .unwrap();
        assert_eq!(
            store
                .remote_live_revision_for_binding(&source, &revisions, RedactionProfile::Redacted,)
                .unwrap(),
            None
        );
        let error = store
            .record_remote_live_state_unfenced(
                &source,
                &revisions,
                RedactionProfile::Redacted,
                &revision_only,
                &[],
                captured_at + Duration::minutes(2),
                captured_at + Duration::minutes(2),
                true,
                &[],
                &[],
            )
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);

        store
            .record_remote_live_state_unfenced(
                &source,
                &revisions,
                RedactionProfile::Redacted,
                &full_live(1, captured_at),
                &[],
                captured_at + Duration::minutes(3),
                captured_at + Duration::minutes(3),
                true,
                &[],
                &[],
            )
            .unwrap();
        assert!(
            store
                .load_remote_live_state(&source.node_id)
                .unwrap()
                .is_some()
        );
    }
}
