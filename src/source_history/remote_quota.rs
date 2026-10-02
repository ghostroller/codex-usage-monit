//! Quota observations stored inside the immutable remote history generation.
//! The enclosing generation lock, writer fence and COW publication own access.

use super::*;
use crate::remote_quota::{RemoteQuotaChange, RemoteQuotaDay};
use std::collections::BTreeSet;

pub(super) const REMOTE_QUOTA_FILE: &str = "quota.json";
pub(super) const MAX_REMOTE_QUOTA_BYTES: u64 = 32 * 1024 * 1024;
// A long-disconnected exporter confirms forward retention time using three
// observations over 48 hours. Until then its journal can retain three separate
// 35-day windows. Do not reject that valid reconnect before tombstones arrive.
const MAX_REMOTE_QUOTA_DAYS: usize = 128;

impl SourceHistoryStore {
    pub(super) fn import_legacy_remote_quota_sqlite(
        &self,
        legacy: &SourceHistoryStore,
        source: &NodeId,
        redaction: RedactionProfile,
        legacy_directory: &Path,
        target_directory: &Path,
    ) -> io::Result<()> {
        let Some(mut history) = read_optional_json_file::<RemoteQuotaHistory>(
            &legacy_directory.join(REMOTE_QUOTA_FILE),
            MAX_REMOTE_QUOTA_BYTES,
        )?
        else {
            return Ok(());
        };
        history.validate(legacy, source, redaction)?;
        let database = self
            .sqlite_database()
            .expect("SQL import requires a database");
        database.write(|connection| {
            let namespace = database.namespace(&target_directory.join(REMOTE_QUOTA_FILE))?;
            for quota in std::mem::take(&mut history.days).into_values() {
                put_sqlite_quota_day(connection, &namespace, &quota)?;
            }
            database::set_state(connection, &namespace, &history)
        })
    }

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
    if let Some(database) = store.sqlite_database() {
        return database.read(|connection| {
            let namespace = database.namespace(&path)?;
            let Some(header) = database::state::<RemoteQuotaHistory>(connection, &namespace)?
            else {
                return Ok(Vec::new());
            };
            validate_sqlite_quota_header(&header, store, source, redaction)?;
            let days: Vec<RemoteQuotaDay> = database::records(
                connection,
                &namespace,
                since
                    .date_naive()
                    .and_hms_opt(0, 0, 0)
                    .expect("UTC midnight exists")
                    .and_utc()
                    .timestamp_millis(),
                budget,
            )?;
            if days.len() > MAX_REMOTE_QUOTA_DAYS {
                return Err(invalid_data("remote SQL quota day count exceeds its bound"));
            }
            let mut points = Vec::new();
            for day in days {
                day.validate()?;
                // The SQL day row already consumed one record budget unit.
                budget.charge_records(day.points.len().saturating_sub(1))?;
                points.extend(
                    day.points
                        .into_iter()
                        .filter(|point| point.observed_at >= since),
                );
            }
            Ok(points)
        });
    }
    store.validate_private_path(directory)?;
    let history = read_optional_json_file_with_budget::<RemoteQuotaHistory>(
        &path,
        MAX_REMOTE_QUOTA_BYTES,
        budget,
    )?;
    let Some(history) = history else {
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
    let update = |mut history: RemoteQuotaHistory| -> io::Result<(RemoteQuotaHistory, bool)> {
        history.validate(store, source, redaction)?;
        let mut changed = false;
        for change in changes {
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
        Ok((history, changed))
    };
    let empty = || RemoteQuotaHistory {
        version: 1,
        profile_id: store.profile_id.clone(),
        source_id: source.clone(),
        redaction_profile: redaction,
        last_sequence: 0,
        days: BTreeMap::new(),
    };
    let path = directory.join(REMOTE_QUOTA_FILE);
    if let Some(database) = store.sqlite_database() {
        return database.write(|connection| {
            let key = database.namespace(&path)?;
            let mut header =
                database::state::<RemoteQuotaHistory>(connection, &key)?.unwrap_or_else(empty);
            validate_sqlite_quota_header(&header, store, source, redaction)?;
            let mut budget = SourceHistoryReadBudget::with_limits(
                MAX_REMOTE_QUOTA_BYTES,
                usize::MAX,
                usize::MAX,
            );
            let days: Vec<RemoteQuotaDay> =
                database::records(connection, &key, i64::MIN, &mut budget)?;
            let mut present = BTreeSet::new();
            let mut sizes = BTreeMap::new();
            for day in days {
                day.validate()?;
                if !present.insert(day.day) {
                    return Err(invalid_data("duplicate remote SQL quota day"));
                }
                sizes.insert(
                    day.day,
                    serde_json::to_vec(&day)
                        .map_err(|error| invalid_data(error.to_string()))?
                        .len() as u64,
                );
            }
            for change in changes {
                if change.sequence.get() <= header.last_sequence {
                    continue;
                }
                if change.quota.points.is_empty() {
                    present.remove(&change.quota.day);
                    sizes.remove(&change.quota.day);
                    database::delete_record(connection, &key, &change.quota.day.to_string())?;
                } else {
                    present.insert(change.quota.day);
                    sizes.insert(
                        change.quota.day,
                        serde_json::to_vec(&change.quota)
                            .map_err(|error| invalid_data(error.to_string()))?
                            .len() as u64,
                    );
                    put_sqlite_quota_day(connection, &key, &change.quota)?;
                }
                header.last_sequence = change.sequence.get();
            }
            if present.len() > MAX_REMOTE_QUOTA_DAYS {
                return Err(invalid_data("remote SQL quota day count exceeds its bound"));
            }
            if sizes
                .values()
                .copied()
                .try_fold(0_u64, u64::checked_add)
                .is_none_or(|bytes| bytes > MAX_REMOTE_QUOTA_BYTES)
            {
                return Err(invalid_data(
                    "remote SQL quota payload exceeds its byte bound",
                ));
            }
            database::set_state(connection, &key, &header)?;
            Ok(())
        });
    }
    let (history, changed) = update(
        read_optional_json_file::<RemoteQuotaHistory>(&path, MAX_REMOTE_QUOTA_BYTES)?
            .unwrap_or_else(empty),
    )?;
    if changed {
        write_private_atomically(
            &path,
            &encode_pretty_bounded(&history, MAX_REMOTE_QUOTA_BYTES)?,
        )?;
    }
    Ok(())
}

fn validate_sqlite_quota_header(
    history: &RemoteQuotaHistory,
    store: &SourceHistoryStore,
    source: &NodeId,
    redaction: RedactionProfile,
) -> io::Result<()> {
    history.validate(store, source, redaction)?;
    if !history.days.is_empty() {
        return Err(invalid_data(
            "remote SQL quota header contains inline day rows",
        ));
    }
    Ok(())
}

fn put_sqlite_quota_day(
    connection: &rusqlite::Connection,
    namespace: &str,
    quota: &RemoteQuotaDay,
) -> io::Result<()> {
    quota.validate()?;
    database::put_record(
        connection,
        namespace,
        &quota.day.to_string(),
        quota
            .day
            .and_hms_opt(0, 0, 0)
            .expect("UTC midnight exists")
            .and_utc()
            .timestamp_millis(),
        quota,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::Provenance;
    use crate::remote_quota::RemoteQuotaPoint;
    use std::num::NonZeroU64;

    #[test]
    fn sqlite_quota_tombstones_keep_other_days_and_full_u64_sequence() {
        let root = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        }
        let store = SourceHistoryStore::new_sqlite(
            root.path().to_owned(),
            "0123456789abcdef".parse().unwrap(),
        );
        let source: NodeId = "node-11111111111111111111111111111111".parse().unwrap();
        let directory = store.profile_directory().join("quota-test");
        let base = DateTime::from_timestamp(1_800_000_000, 0).unwrap();
        let quota = |observed_at: DateTime<Utc>| RemoteQuotaDay {
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
        };
        let first = RemoteQuotaChange {
            sequence: NonZeroU64::new(u64::MAX - 2).unwrap(),
            quota: quota(base),
        };
        let second = RemoteQuotaChange {
            sequence: NonZeroU64::new(u64::MAX - 1).unwrap(),
            quota: quota(base + chrono::Duration::days(1)),
        };
        apply_remote_quota(
            &store,
            &source,
            RedactionProfile::Redacted,
            &directory,
            &[first.clone(), second.clone()],
        )
        .unwrap();
        let remove = RemoteQuotaChange {
            sequence: NonZeroU64::new(u64::MAX).unwrap(),
            quota: RemoteQuotaDay {
                day: first.quota.day,
                points: Vec::new(),
            },
        };
        apply_remote_quota(
            &store,
            &source,
            RedactionProfile::Redacted,
            &directory,
            &[remove],
        )
        .unwrap();
        apply_remote_quota(
            &store,
            &source,
            RedactionProfile::Redacted,
            &directory,
            &[first, second.clone()],
        )
        .unwrap();
        let mut budget = SourceHistoryReadBudget::for_query();
        let points = read_remote_quota(
            &store,
            &source,
            RedactionProfile::Redacted,
            &directory,
            base,
            &mut budget,
        )
        .unwrap();
        assert_eq!(points, second.quota.points);
        let db = store.sqlite_database().unwrap();
        db.read(|connection| {
            let key = db.namespace(&directory.join(REMOTE_QUOTA_FILE))?;
            let header: RemoteQuotaHistory = database::state(connection, &key)?.unwrap();
            assert_eq!(header.last_sequence, u64::MAX);
            assert!(header.days.is_empty());
            let mut budget = SourceHistoryReadBudget::for_query();
            assert_eq!(
                database::records::<RemoteQuotaDay>(connection, &key, i64::MIN, &mut budget)?.len(),
                1
            );
            Ok(())
        })
        .unwrap();
        assert!(!directory.exists());
    }

    #[test]
    fn quota_history_buffers_long_offline_reconnection_during_retention_confirmation() {
        let root = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        }
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
