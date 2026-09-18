//! Quota observations stored inside the immutable remote history generation.
//! The enclosing generation lock, writer fence and COW publication own access.

use super::*;
use crate::remote_quota::{RemoteQuotaChange, RemoteQuotaDay};

pub(super) const REMOTE_QUOTA_FILE: &str = "quota.json";
pub(super) const MAX_REMOTE_QUOTA_BYTES: u64 = 32 * 1024 * 1024;
// A long-disconnected exporter confirms forward retention time using three
// observations over 48 hours. Until then its journal can retain three separate
// 35-day windows. Do not reject that valid reconnect before tombstones arrive.
const MAX_REMOTE_QUOTA_DAYS: usize = 128;

impl SourceHistoryStore {
    pub(crate) fn load_remote_quota_since_with_budget(
        &self,
        source: &NodeId,
        redaction: RedactionProfile,
        since: DateTime<Utc>,
        budget: &mut SourceHistoryReadBudget,
    ) -> io::Result<Vec<QuotaPoint>> {
        budget.charge_source()?;
        self.with_source_metadata_shared(source, |metadata| {
            if metadata.kind() != SourceKind::Ssh {
                return Err(invalid_data("remote quota requires an SSH source"));
            }
            self.with_active_remote_history_generation(source, redaction, |directory| {
                let Some(directory) = directory else {
                    return Ok(Vec::new());
                };
                Ok(
                    read_remote_quota(self, source, redaction, directory, since, budget)?
                        .iter()
                        .map(crate::remote_quota::RemoteQuotaPoint::to_local)
                        .collect(),
                )
            })
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RemoteQuotaHistory {
    version: u32,
    profile_id: HistoryProfileId,
    source_id: NodeId,
    redaction_profile: RedactionProfile,
    last_sequence: u64,
    days: BTreeMap<NaiveDate, RemoteQuotaDay>,
}

impl RemoteQuotaHistory {
    fn validate(
        &self,
        store: &SourceHistoryStore,
        source: &NodeId,
        redaction: RedactionProfile,
    ) -> io::Result<()> {
        if self.version != 1
            || self.profile_id != store.profile_id
            || self.source_id != *source
            || self.redaction_profile != redaction
            || self.days.len() > MAX_REMOTE_QUOTA_DAYS
        {
            return Err(invalid_data(
                "remote quota history binding or day bound is invalid",
            ));
        }
        for (day, quota) in &self.days {
            if *day != quota.day {
                return Err(invalid_data("remote quota day key mismatch"));
            }
            quota.validate()?;
        }
        Ok(())
    }
}

pub(super) fn validate_remote_quota_file(
    store: &SourceHistoryStore,
    source: &NodeId,
    redaction: RedactionProfile,
    path: &Path,
) -> io::Result<()> {
    let history: RemoteQuotaHistory = read_json_file(path, MAX_REMOTE_QUOTA_BYTES)?;
    history.validate(store, source, redaction)
}

pub(super) fn read_remote_quota(
    store: &SourceHistoryStore,
    source: &NodeId,
    redaction: RedactionProfile,
    directory: &Path,
    since: DateTime<Utc>,
    budget: &mut SourceHistoryReadBudget,
) -> io::Result<Vec<crate::remote_quota::RemoteQuotaPoint>> {
    let path = directory.join(REMOTE_QUOTA_FILE);
    store.validate_private_path(directory)?;
    let Some(history) = read_optional_json_file_with_budget::<RemoteQuotaHistory>(
        &path,
        MAX_REMOTE_QUOTA_BYTES,
        budget,
    )?
    else {
        return Ok(Vec::new());
    };
    history.validate(store, source, redaction)?;
    budget.charge_records(history.days.values().map(|day| day.points.len()).sum())?;
    let points = history
        .days
        .into_values()
        .flat_map(|day| day.points)
        .filter(|point| point.observed_at >= since)
        .collect::<Vec<_>>();
    Ok(points)
}

pub(super) fn apply_remote_quota(
    store: &SourceHistoryStore,
    source: &NodeId,
    redaction: RedactionProfile,
    directory: &Path,
    changes: &[RemoteQuotaChange],
) -> io::Result<()> {
    if changes.is_empty() {
        return Ok(());
    }
    for change in changes {
        change.quota.validate()?;
    }
    if changes
        .windows(2)
        .any(|changes| changes[0].sequence >= changes[1].sequence)
    {
        return Err(invalid_data(
            "remote quota changes are not in journal order",
        ));
    }
    let path = directory.join(REMOTE_QUOTA_FILE);
    let mut history = read_optional_json_file::<RemoteQuotaHistory>(&path, MAX_REMOTE_QUOTA_BYTES)?
        .unwrap_or_else(|| RemoteQuotaHistory {
            version: 1,
            profile_id: store.profile_id.clone(),
            source_id: source.clone(),
            redaction_profile: redaction,
            last_sequence: 0,
            days: BTreeMap::new(),
        });
    history.validate(store, source, redaction)?;
    let mut changed = false;
    for change in changes {
        // The ingest WAL binds the exact complete page before this function
        // runs. Its replay may repeat an already-applied journal prefix.
        if change.sequence.get() <= history.last_sequence {
            continue;
        }
        if change.quota.points.is_empty() {
            history.days.remove(&change.quota.day);
        } else {
            history.days.insert(change.quota.day, change.quota.clone());
        }
        history.last_sequence = change.sequence.get();
        changed = true;
    }
    history.validate(store, source, redaction)?;
    if changed {
        write_private_atomically(
            &path,
            &encode_pretty_bounded(&history, MAX_REMOTE_QUOTA_BYTES)?,
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::Provenance;
    use crate::remote_quota::RemoteQuotaPoint;
    use std::num::NonZeroU64;

    #[test]
    fn quota_history_buffers_long_offline_reconnection_during_retention_confirmation() {
        let root = tempfile::tempdir().unwrap();
        let store =
            SourceHistoryStore::new(root.path().to_owned(), "0123456789abcdef".parse().unwrap());
        let source: NodeId = "node-11111111111111111111111111111111".parse().unwrap();
        let directory = root.path().join("quota-test");
        store.prepare_private_directory(&directory).unwrap();
        let base = DateTime::from_timestamp(1_800_000_000, 0).unwrap();
        let changes = (0..105)
            .map(|index| {
                let observed_at = base + chrono::Duration::days(index);
                RemoteQuotaChange {
                    sequence: NonZeroU64::new(index as u64 + 1).unwrap(),
                    quota: RemoteQuotaDay {
                        day: observed_at.date_naive(),
                        points: vec![
                            RemoteQuotaPoint::from_local(&QuotaPoint {
                                observed_at,
                                limit_id: "codex".into(),
                                duration_mins: 300,
                                resets_at: observed_at + chrono::Duration::hours(4),
                                used_percent: 10.0,
                                remaining_percent: 90.0,
                                provenance: Provenance::ServerSnapshot,
                            })
                            .unwrap(),
                        ],
                    },
                }
            })
            .collect::<Vec<_>>();
        for window in changes.chunks(35) {
            apply_remote_quota(
                &store,
                &source,
                RedactionProfile::Redacted,
                &directory,
                window,
            )
            .unwrap();
        }
        let recent = read_remote_quota(
            &store,
            &source,
            RedactionProfile::Redacted,
            &directory,
            base + chrono::Duration::days(70),
            &mut SourceHistoryReadBudget::for_query(),
        )
        .unwrap();
        assert_eq!(recent.len(), 35);
        let tombstones = changes[..70]
            .iter()
            .enumerate()
            .map(|(index, change)| RemoteQuotaChange {
                sequence: NonZeroU64::new(106 + index as u64).unwrap(),
                quota: RemoteQuotaDay {
                    day: change.quota.day,
                    points: Vec::new(),
                },
            })
            .collect::<Vec<_>>();
        apply_remote_quota(
            &store,
            &source,
            RedactionProfile::Redacted,
            &directory,
            &tombstones,
        )
        .unwrap();
        let retained = read_remote_quota(
            &store,
            &source,
            RedactionProfile::Redacted,
            &directory,
            base,
            &mut SourceHistoryReadBudget::for_query(),
        )
        .unwrap();
        assert_eq!(retained, recent);
    }
}
