//! Server quota observations in SQL, including samples retained independently
//! from a fresh remote bootstrap. Page data and cursor publication share a transaction.

use super::*;
use crate::remote_quota::{RemoteQuotaChange, RemoteQuotaDay};

pub(super) const REMOTE_QUOTA_FILE: &str = "quota.json";
pub(super) const MAX_REMOTE_QUOTA_BYTES: u64 = 32 * 1024 * 1024;
// A long-disconnected exporter confirms forward retention time using three
// observations over 48 hours. Until then its journal can retain three separate
// 35-day windows. Do not reject that valid reconnect before tombstones arrive.
const MAX_REMOTE_QUOTA_DAYS: usize = 128;

impl SourceHistoryStore {
    fn retained_remote_quota_directory(
        &self,
        source: &NodeId,
        redaction: RedactionProfile,
    ) -> PathBuf {
        self.source_directory(source)
            .join(redaction.directory_name())
            .join("retained-quota")
    }

    /// Retains validated server samples independently from a new remote bootstrap.
    /// The initialization caller holds the ownership leases and outer SQL transaction.
    pub(crate) fn save_retained_remote_quota_points_unfenced(
        &self,
        source: &NodeId,
        redaction: RedactionProfile,
        points: &[QuotaPoint],
    ) -> io::Result<()> {
        for point in points {
            validate_account_quota_point(point)?;
        }
        let database = self.sqlite_database().expect("SQLite history backend");
        database.write(|connection| {
            if self.load_source_metadata(source)?.kind() != SourceKind::Ssh {
                return Err(invalid_data("retained remote quota requires an SSH source"));
            }
            let namespace =
                database.namespace(&self.retained_remote_quota_directory(source, redaction))?;
            let mut changed = false;
            for point in points {
                let key = sqlite_quota_key(point)?;
                let previous: Option<QuotaPoint> = sqlite_record(connection, &namespace, &key)?;
                if let Some(previous) = &previous {
                    validate_account_quota_point(previous)?;
                }
                if previous.as_ref() == Some(point) {
                    continue;
                }
                database::put_record(
                    connection,
                    &namespace,
                    &key,
                    point.observed_at.timestamp_millis(),
                    point,
                )?;
                changed = true;
            }
            if changed {
                self.advance_remote_history_projection_revision(connection, source, redaction)?;
            }
            Ok(())
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
            let database = self.sqlite_database().expect("SQLite history backend");
            database.read(|connection| {
                let namespace =
                    database.namespace(&self.retained_remote_quota_directory(source, redaction))?;
                let mut points: Vec<QuotaPoint> =
                    database::records(connection, &namespace, since.timestamp_millis(), budget)?;
                points.retain(|point| point.observed_at >= since);
                for point in &points {
                    validate_account_quota_point(point)?;
                }
                let mut index = account_quota_point_index(&points)?;
                self.with_active_remote_history_generation(source, redaction, |directory| {
                    if let Some(directory) = directory {
                        for point in
                            read_remote_quota(self, source, redaction, directory, since, budget)?
                        {
                            apply_account_quota_point(&mut points, &mut index, point.to_local())?;
                        }
                    }
                    points.sort_by(|a, b| {
                        a.observed_at
                            .cmp(&b.observed_at)
                            .then_with(|| a.limit_id.cmp(&b.limit_id))
                            .then(a.duration_mins.cmp(&b.duration_mins))
                            .then(a.resets_at.cmp(&b.resets_at))
                    });
                    Ok(points)
                })
            })
        })
    }

    #[cfg(test)]
    pub(crate) fn load_remote_quota_since(
        &self,
        source: &NodeId,
        redaction: RedactionProfile,
        since: DateTime<Utc>,
    ) -> io::Result<Vec<QuotaPoint>> {
        self.load_remote_quota_since_with_budget(
            source,
            redaction,
            since,
            &mut SourceHistoryReadBudget::for_query(),
        )
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

pub(super) fn read_remote_quota(
    store: &SourceHistoryStore,
    source: &NodeId,
    redaction: RedactionProfile,
    directory: &Path,
    since: DateTime<Utc>,
    budget: &mut SourceHistoryReadBudget,
) -> io::Result<Vec<crate::remote_quota::RemoteQuotaPoint>> {
    let path = directory.join(REMOTE_QUOTA_FILE);
    let database = store.sqlite_database().expect("SQLite history backend");

    database.read(|connection| {
        let namespace = database.namespace(&path)?;
        let Some(header) = database::state::<RemoteQuotaHistory>(connection, &namespace)? else {
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
    })
}

pub(super) fn apply_remote_quota(
    store: &SourceHistoryStore,
    source: &NodeId,
    redaction: RedactionProfile,
    directory: &Path,
    changes: &[RemoteQuotaChange],
) -> io::Result<bool> {
    if changes.is_empty() {
        return Ok(false);
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
    let empty = || RemoteQuotaHistory {
        version: 1,
        profile_id: store.profile_id.clone(),
        source_id: source.clone(),
        redaction_profile: redaction,
        last_sequence: 0,
        days: BTreeMap::new(),
    };
    let path = directory.join(REMOTE_QUOTA_FILE);
    let database = store.sqlite_database().expect("SQLite history backend");

    database.write(|connection| {
        let key = database.namespace(&path)?;
        let mut header =
            database::state::<RemoteQuotaHistory>(connection, &key)?.unwrap_or_else(empty);
        validate_sqlite_quota_header(&header, store, source, redaction)?;
        let mut budget =
            SourceHistoryReadBudget::with_limits(MAX_REMOTE_QUOTA_BYTES, usize::MAX, usize::MAX);
        let days: Vec<RemoteQuotaDay> = database::records(connection, &key, i64::MIN, &mut budget)?;
        let mut present = BTreeMap::new();
        let mut sizes = BTreeMap::new();
        for day in days {
            day.validate()?;
            if present.insert(day.day, day.clone()).is_some() {
                return Err(invalid_data("duplicate remote SQL quota day"));
            }
            sizes.insert(
                day.day,
                serde_json::to_vec(&day)
                    .map_err(|error| invalid_data(error.to_string()))?
                    .len() as u64,
            );
        }
        let mut changed = false;
        for change in changes {
            if change.sequence.get() <= header.last_sequence {
                continue;
            }
            if change.quota.points.is_empty() {
                changed |= present.remove(&change.quota.day).is_some();
                sizes.remove(&change.quota.day);
                database::delete_record(connection, &key, &change.quota.day.to_string())?;
            } else {
                changed |= present.get(&change.quota.day) != Some(&change.quota);
                present.insert(change.quota.day, change.quota.clone());
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
        Ok(changed)
    })
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
    use super::super::redaction_retirement::SourceRedactionRetirementStatus;
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
        let directory = store
            .source_directory(&source)
            .join(RedactionProfile::Redacted.directory_name())
            .join("quota-test");
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
    #[test]
    fn retained_quota_is_visible_without_bootstrap_and_merges_with_current_samples() {
        use crate::remote_protocol::{ProtocolRevisions, SourceGeneration};
        use std::num::NonZeroU32;
        let root = tempfile::tempdir().unwrap();
        let ownership = crate::history_ownership::HistoryOwnershipStore::new(
            root.path().join("state"),
            "profile".parse().unwrap(),
            RedactionProfile::Redacted,
        );
        let (manifest, store) =
            crate::sqlite_history_initialization::initialize_for_test(&ownership).unwrap();
        let source: NodeId = "node-11111111111111111111111111111111".parse().unwrap();
        let redaction = RedactionProfile::Redacted;
        store
            .save_source_metadata(
                &SourceMetadata::new_with_redaction_profile(
                    source.clone(),
                    SourceKind::Ssh,
                    "remote",
                    redaction,
                )
                .unwrap(),
            )
            .unwrap();
        let base = DateTime::from_timestamp(1_800_000_000, 0).unwrap();
        let point = QuotaPoint {
            observed_at: base,
            limit_id: "codex".into(),
            duration_mins: 300,
            resets_at: base + chrono::Duration::hours(4),
            used_percent: 10.0,
            remaining_percent: 90.0,
            provenance: Provenance::ServerSnapshot,
        };
        store
            .save_retained_remote_quota_points_unfenced(
                &source,
                redaction,
                std::slice::from_ref(&point),
            )
            .unwrap();
        let load = |profile| {
            store
                .load_remote_quota_since_with_budget(
                    &source,
                    profile,
                    base,
                    &mut SourceHistoryReadBudget::for_query(),
                )
                .unwrap()
        };
        assert_eq!(load(redaction), vec![point.clone()]);
        assert!(load(RedactionProfile::PreviewEnabled).is_empty());
        let generation = "ingest-gen-11111111111111111111111111111111"
            .parse()
            .unwrap();
        let one = NonZeroU32::new(1).unwrap();
        let binding = SourceHistoryRemoteBinding::new(
            SourceGeneration {
                node_id: source.clone(),
                generation: NonZeroU64::new(1).unwrap(),
            },
            ProtocolRevisions {
                history_format: one,
                metric: one,
                estimator: one,
                project_breakdown: one,
                api_pricing_catalog: one,
                model_catalog_fingerprint: crate::remote_protocol::test_model_catalog_fingerprint(
                    1,
                ),
            },
        )
        .unwrap();
        store
            .ensure_remote_history_generation(&source, redaction, &generation, &binding)
            .unwrap();
        let mut latest = point.clone();
        latest.used_percent = 20.0;
        latest.remaining_percent = 80.0;
        let changes = [RemoteQuotaChange {
            sequence: NonZeroU64::new(1).unwrap(),
            quota: RemoteQuotaDay {
                day: base.date_naive(),
                points: vec![RemoteQuotaPoint::from_local(&latest).unwrap()],
            },
        }];
        store
            .apply_remote_history_generation_page(
                &source,
                redaction,
                &generation,
                &binding,
                &[],
                &[],
                &changes,
            )
            .unwrap();
        store
            .activate_remote_history_generation(
                &source,
                redaction,
                None,
                &generation,
                &binding,
                base,
            )
            .unwrap();
        assert_eq!(load(redaction), vec![latest]);
        store
            .update_source_metadata(&source, |metadata| {
                metadata.set_detached(true);
                Ok(())
            })
            .unwrap();
        let lease = ownership.acquire_writer_lease().unwrap();
        let authority = ownership.authorize_v2_write(&lease, &manifest).unwrap();
        store
            .writer(&authority)
            .unwrap()
            .purge_detached_ssh_source(&source)
            .unwrap();
        let db = store.sqlite_database().unwrap();
        assert!(
            db.read(|connection| database::records::<QuotaPoint>(
                connection,
                &db.namespace(&store.retained_remote_quota_directory(&source, redaction))?,
                i64::MIN,
                &mut SourceHistoryReadBudget::for_query()
            ))
            .unwrap()
            .is_empty()
        );
    }

    #[test]
    fn retained_quota_gc_and_privacy_retirement_are_scoped_and_invalidate_projection() {
        let root = tempfile::tempdir().unwrap();
        let ownership = crate::history_ownership::HistoryOwnershipStore::new(
            root.path().join("state"),
            "profile".parse().unwrap(),
            RedactionProfile::Redacted,
        );
        let (manifest, store) =
            crate::sqlite_history_initialization::initialize_for_test(&ownership).unwrap();
        let source: NodeId = "node-11111111111111111111111111111111".parse().unwrap();
        store
            .save_source_metadata(
                &SourceMetadata::new_with_redaction_profile(
                    source.clone(),
                    SourceKind::Ssh,
                    "remote",
                    RedactionProfile::PreviewEnabled,
                )
                .unwrap(),
            )
            .unwrap();
        let base = DateTime::from_timestamp(1_800_000_000, 0).unwrap();
        store.garbage_collect(base).unwrap();
        let point = |observed_at| QuotaPoint {
            observed_at,
            limit_id: "codex".into(),
            duration_mins: 300,
            resets_at: observed_at + chrono::Duration::hours(4),
            used_percent: 10.0,
            remaining_percent: 90.0,
            provenance: Provenance::ServerSnapshot,
        };
        let old = point(base - chrono::Duration::days(40));
        let recent = point(base);
        store
            .save_retained_remote_quota_points_unfenced(
                &source,
                RedactionProfile::PreviewEnabled,
                &[old],
            )
            .unwrap();
        store
            .save_retained_remote_quota_points_unfenced(
                &source,
                RedactionProfile::Redacted,
                std::slice::from_ref(&recent),
            )
            .unwrap();
        let before = store
            .load_remote_history_projection_revision(&source, RedactionProfile::PreviewEnabled)
            .unwrap();
        assert_eq!(store.garbage_collect(base).unwrap().shards_pruned, 1);
        assert!(
            store
                .load_remote_quota_since(
                    &source,
                    RedactionProfile::PreviewEnabled,
                    base - chrono::Duration::days(60)
                )
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            store
                .load_remote_history_projection_revision(&source, RedactionProfile::PreviewEnabled)
                .unwrap(),
            before + 1
        );
        assert_eq!(
            store
                .load_remote_quota_since(&source, RedactionProfile::Redacted, base)
                .unwrap(),
            vec![recent.clone()]
        );
        store
            .save_retained_remote_quota_points_unfenced(
                &source,
                RedactionProfile::PreviewEnabled,
                std::slice::from_ref(&recent),
            )
            .unwrap();
        let lease = ownership.acquire_writer_lease().unwrap();
        let authority = ownership.authorize_v2_write(&lease, &manifest).unwrap();
        let (_, status) = store
            .writer(&authority)
            .unwrap()
            .publish_remote_source_redaction_profile(&source, RedactionProfile::Redacted)
            .unwrap();
        assert_eq!(status, SourceRedactionRetirementStatus::Complete);
        assert!(
            store
                .load_remote_quota_since(&source, RedactionProfile::PreviewEnabled, base)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            store
                .load_remote_quota_since(&source, RedactionProfile::Redacted, base)
                .unwrap(),
            vec![recent]
        );
    }
}
