//! One-time retention of settings, purge intent, revision floors and sampled quota.
//! No old usage bucket, digest, facts, pending publication or cursor is read.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use chrono::NaiveDate;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::history::QuotaPoint;
use crate::remote_quota::RemoteQuotaDay;
use crate::source_history::database;
use crate::source_history::{
    HistoryProfileId, RedactionProfile, SourceHistoryRemoteGenerationId, SourceHistoryStore,
    SourceKind, SourceMetadata, SourcePurgeMarker,
};
use crate::source_identity::{NodeId, SourceIdentityStore};

const MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;
const MAX_INPUT_BYTES: u64 = 512 * 1024 * 1024;
const MAX_INPUT_FILES: usize = 4096;
const MAX_POINTS: usize = 1_000_000;

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[derive(Default)]
struct InputBudget {
    bytes: u64,
    files: usize,
    points: usize,
}

impl InputBudget {
    fn points(&mut self, count: usize) -> io::Result<()> {
        self.points = self
            .points
            .checked_add(count)
            .ok_or_else(|| invalid("quota point count overflow"))?;
        if self.points > MAX_POINTS {
            return Err(invalid("retained quota exceeds its point budget"));
        }
        Ok(())
    }
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SourcePolicy {
    format_version: u32,
    profile_id: HistoryProfileId,
    source: SourceMetadata,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RevisionFloor {
    format_version: u32,
    profile_id: HistoryProfileId,
    source_id: NodeId,
    source_generation: u64,
    redaction_profile: RedactionProfile,
    last_reserved_revision: u64,
}

// Unknown fields are skipped by serde, including all old usage payloads.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AccountQuota {
    format_version: u32,
    quota_revision: u32,
    profile_id: HistoryProfileId,
    utc_day: NaiveDate,
    quota_points: Vec<QuotaPoint>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct V1Quota {
    format_version: u32,
    namespace: String,
    utc_day: NaiveDate,
    #[serde(default)]
    quota_points: Vec<QuotaPoint>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ActiveQuotaGeneration {
    format_version: u32,
    profile_id: HistoryProfileId,
    source_id: NodeId,
    redaction_profile: RedactionProfile,
    active_generation: SourceHistoryRemoteGenerationId,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RemoteQuota {
    version: u32,
    profile_id: HistoryProfileId,
    source_id: NodeId,
    redaction_profile: RedactionProfile,
    days: BTreeMap<NaiveDate, RemoteQuotaDay>,
}

/// The caller holds both ownership leases and a single SQLite transaction.
pub(crate) fn retain_required_state(store: &SourceHistoryStore) -> io::Result<()> {
    let database = store.sqlite_database().expect("initialization uses SQLite");
    let identity =
        SourceIdentityStore::at_path(store.state_root().join("source-identity.json")).load()?;
    let mut budget = InputBudget::default();
    database.write(|connection| {
        for source_directory in directories(store, &store.sources_directory())? {
            let Some(name) = source_directory.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            let source_id = match name.parse::<NodeId>() {
                Ok(id) => id,
                Err(_) if name.starts_with("node-") => {
                    return Err(invalid("malformed retained source ID"));
                }
                Err(_) => continue,
            };
            // An irreversible claim may outlive its source metadata after a
            // crash. Preserve the user's deletion intent independently of
            // whether settings or retained quota are still present.
            let purge_path = source_directory.join("source-purge.json");
            let purge = read::<SourcePurgeMarker>(store, &purge_path, &mut budget)?;
            if let Some(marker) = &purge {
                marker.validate(store.profile_id(), &source_id)?;
                database::set_state(connection, &database.namespace(&purge_path)?, marker)?;
            }
            if let Some(policy) =
                read::<SourcePolicy>(store, &source_directory.join("source.json"), &mut budget)?
            {
                if policy.format_version != 1
                    || policy.profile_id != *store.profile_id()
                    || policy.source.source_id() != &source_id
                {
                    return Err(invalid("retained source policy binding mismatch"));
                }
                policy.source.validate()?;
                if purge.is_some()
                    && (policy.source.kind() != SourceKind::Ssh || !policy.source.detached())
                {
                    return Err(invalid("retained purge claim conflicts with source policy"));
                }
                let key = database.namespace(&source_directory.join("source.json"))?;
                database::set_state(connection, &key, &policy)?;
                if policy.source.kind() == SourceKind::Ssh {
                    for redaction in [RedactionProfile::Redacted, RedactionProfile::PreviewEnabled]
                    {
                        retain_remote_quota(store, &source_id, redaction, &mut budget)?;
                    }
                }
            }
        }
        for redaction in [RedactionProfile::Redacted, RedactionProfile::PreviewEnabled] {
            let path = store
                .source_directory(identity.node_id())
                .join(redaction.directory_name())
                .join("local-observation-state.json");
            if let Some(floor) = read::<RevisionFloor>(store, &path, &mut budget)? {
                if floor.format_version != 1
                    || floor.profile_id != *store.profile_id()
                    || floor.source_id != *identity.node_id()
                    || floor.source_generation != identity.generation()
                    || floor.redaction_profile != redaction
                {
                    return Err(invalid("retained local revision binding mismatch"));
                }
                database::set_state(connection, &database.namespace(&path)?, &floor)?;
            }
            let namespace = match redaction {
                RedactionProfile::Redacted => format!("{}-redacted", store.profile_id()),
                RedactionProfile::PreviewEnabled => store.profile_id().to_string(),
            };
            let directory = store.state_root().join("history-v1").join(&namespace);
            for (path, day) in day_files(store, &directory)? {
                let quota: V1Quota = read(store, &path, &mut budget)?
                    .ok_or_else(|| invalid("retained quota disappeared"))?;
                if !matches!(quota.format_version, 1 | 2)
                    || quota.namespace != namespace
                    || quota.utc_day != day
                {
                    return Err(invalid("retained v1 quota namespace mismatch"));
                }
                retain_account_points(store, day, &quota.quota_points, &mut budget)?;
            }
        }
        for (path, day) in day_files(store, &store.account_directory())? {
            let quota: AccountQuota = read(store, &path, &mut budget)?
                .ok_or_else(|| invalid("retained account quota disappeared"))?;
            if quota.format_version != 1
                || quota.quota_revision != 1
                || quota.profile_id != *store.profile_id()
                || quota.utc_day != day
            {
                return Err(invalid("retained account quota binding mismatch"));
            }
            retain_account_points(store, day, &quota.quota_points, &mut budget)?;
        }
        Ok(())
    })
}

fn retain_account_points(
    store: &SourceHistoryStore,
    day: NaiveDate,
    points: &[QuotaPoint],
    budget: &mut InputBudget,
) -> io::Result<()> {
    budget.points(points.len())?;
    for point in points {
        crate::source_history::validate_account_quota_point(point)?;
        if point.observed_at.date_naive() != day {
            return Err(invalid("retained quota observation is outside its UTC day"));
        }
    }
    store.record_account_points_unfenced(points)?;
    Ok(())
}

fn retain_remote_quota(
    store: &SourceHistoryStore,
    source: &NodeId,
    redaction: RedactionProfile,
    budget: &mut InputBudget,
) -> io::Result<()> {
    let root = store
        .source_directory(source)
        .join(redaction.directory_name())
        .join("remote-history-v1");
    let Some(active) = read::<ActiveQuotaGeneration>(store, &root.join("active.json"), budget)?
    else {
        return Ok(());
    };
    if active.format_version != 2
        || active.profile_id != *store.profile_id()
        || active.source_id != *source
        || active.redaction_profile != redaction
    {
        return Err(invalid("retained remote quota active binding mismatch"));
    }
    let path = root
        .join("generations")
        .join(active.active_generation.as_str())
        .join("quota.json");
    let Some(quota) = read::<RemoteQuota>(store, &path, budget)? else {
        return Ok(());
    };
    if quota.version != 1
        || quota.profile_id != *store.profile_id()
        || quota.source_id != *source
        || quota.redaction_profile != redaction
        || quota.days.len() > 128
    {
        return Err(invalid("retained remote quota binding mismatch"));
    }
    let mut points = Vec::new();
    for (day, quota) in quota.days {
        quota.validate()?;
        if quota.day != day {
            return Err(invalid("retained remote quota day mismatch"));
        }
        budget.points(quota.points.len())?;
        points.extend(quota.points.iter().map(|point| point.to_local()));
    }
    store.save_retained_remote_quota_points_unfenced(source, redaction, &points)
}

fn directories(store: &SourceHistoryStore, path: &Path) -> io::Result<Vec<PathBuf>> {
    match store.validate_private_path(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    }
    let mut paths = Vec::new();
    for entry in fs::read_dir(path)? {
        if paths.len() >= MAX_INPUT_FILES {
            return Err(invalid("retained settings directory exceeds entry budget"));
        }
        paths.push(entry?.path());
    }
    paths.sort();
    Ok(paths)
}

fn day_files(
    store: &SourceHistoryStore,
    directory: &Path,
) -> io::Result<Vec<(PathBuf, NaiveDate)>> {
    let mut paths = Vec::new();
    for path in directories(store, directory)? {
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|value| value.to_str()) else {
            continue;
        };
        if let Ok(day) = NaiveDate::parse_from_str(stem, "%Y-%m-%d") {
            paths.push((path, day));
        }
    }
    Ok(paths)
}

fn read<T: DeserializeOwned>(
    store: &SourceHistoryStore,
    path: &Path,
    budget: &mut InputBudget,
) -> io::Result<Option<T>> {
    let parent = path
        .parent()
        .ok_or_else(|| invalid("retained input has no parent"))?;
    match store.validate_private_path(parent) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    }
    let before = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    crate::source_history::validate_data_file_metadata(path, &before)?;
    if before.len() > MAX_FILE_BYTES {
        return Err(invalid("retained input exceeds file budget"));
    }
    let mut options = OpenOptions::new();
    options.read(true);
    crate::source_history::add_nofollow_flags(&mut options);
    let mut file = options.open(path)?;
    crate::source_history::validate_data_file_metadata(path, &file.metadata()?)?;
    crate::source_history::ensure_opened_file_matches_path(
        path,
        &file,
        &before,
        &file.metadata()?,
        "retained input",
    )?;
    let mut bytes = Vec::new();
    file.by_ref()
        .take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(invalid("retained input grew beyond file budget"));
    }
    budget.bytes = budget
        .bytes
        .checked_add(bytes.len() as u64)
        .ok_or_else(|| invalid("retained input size overflow"))?;
    budget.files += 1;
    if budget.bytes > MAX_INPUT_BYTES || budget.files > MAX_INPUT_FILES {
        return Err(invalid("retained state exceeds input budget"));
    }
    store.validate_private_path(parent)?;
    let after = fs::symlink_metadata(path)?;
    crate::source_history::validate_data_file_metadata(path, &after)?;
    crate::source_history::ensure_opened_file_matches_path(
        path,
        &file,
        &after,
        &file.metadata()?,
        "retained input",
    )?;
    serde_json::from_slice(&bytes).map(Some).map_err(|error| {
        invalid(format!(
            "invalid retained input {}: {error}",
            path.display()
        ))
    })
}
