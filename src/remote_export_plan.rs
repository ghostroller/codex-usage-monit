//! Pure planning from one normalized source observation to the durable
//! materialized export set.
//!
//! Complete observations can be planned directly. Partial observations must
//! pass the per-key lower-bound filter before publication: `UpsertOnly` alone
//! could replace a complete key with a smaller aggregate. One-shot scans use
//! `UpsertOnly`; `Authoritative` additionally requires durable continuous
//! coverage that the short-lived exporter does not yet claim.

use std::collections::{BTreeMap, BTreeSet};
use std::io;

use chrono::Days;

use crate::remote_delta_journal::{RemoteDeltaJournalRecord, encode_remote_delta_journal_record};
use crate::remote_export_state::RemoteExportDesiredRecord;
use crate::remote_protocol::{
    RemoteProjectDescriptor, RemoteSessionDigest, RemoteSessionDigestMutation, RemoteUsageBucket,
    RemoteUsageBucketMutation,
};
use crate::source_export::MaterializedSourceObservation;
use crate::source_model::ObservedProjectKey;

mod partial;
pub(crate) use partial::filter_partial_export_records;

const SESSION_DIGEST_RETENTION_DAYS: u64 = 35;
const USAGE_BUCKET_RETENTION_DAYS: u64 = 35;

/// Builds the canonical desired aggregate set for one normalized collection.
///
/// Logical keys contain no path or preview content. Upsert identities are
/// derived solely from their canonical wire payload, so rerunning an
/// unchanged collection is a durable no-op. Tombstones are prepared alongside
/// each current record and are emitted later only by an authoritative
/// reconcile.
pub fn plan_remote_export_records(
    materialized: &MaterializedSourceObservation,
) -> io::Result<Vec<RemoteExportDesiredRecord>> {
    let descriptors = descriptor_index(&materialized.project_descriptors)?;
    let mut records = BTreeMap::<String, RemoteExportDesiredRecord>::new();

    for bucket in &materialized.buckets {
        insert_unique_record(&mut records, plan_bucket_record(bucket, &descriptors)?)?;
    }
    for digest in &materialized.session_digests {
        insert_unique_record(
            &mut records,
            plan_session_digest_record(digest, &descriptors)?,
        )?;
    }

    Ok(records.into_values().collect())
}

fn plan_bucket_record(
    bucket: &RemoteUsageBucket,
    descriptors: &BTreeMap<ObservedProjectKey, RemoteProjectDescriptor>,
) -> io::Result<RemoteExportDesiredRecord> {
    let logical_key = format!("bucket-v1:{}", bucket.starts_at.timestamp());
    let expires_at = bucket
        .ends_at
        .checked_add_days(Days::new(USAGE_BUCKET_RETENTION_DAYS))
        .ok_or_else(|| invalid_data("remote usage bucket retention bound overflows"))?;
    let referenced = bucket
        .project_groups
        .iter()
        .filter_map(|group| group.observed_project_key.clone())
        .collect::<BTreeSet<_>>();
    let upsert = encode_remote_delta_journal_record(
        RemoteDeltaJournalRecord::UsageBucket {
            starts_at: bucket.starts_at,
            mutation: RemoteUsageBucketMutation::Upsert(Box::new(bucket.clone())),
        },
        descriptors_for_keys(&referenced, descriptors)?,
    )?;
    let tombstone = encode_remote_delta_journal_record(
        RemoteDeltaJournalRecord::UsageBucket {
            starts_at: bucket.starts_at,
            mutation: RemoteUsageBucketMutation::Tombstone,
        },
        Vec::new(),
    )?;
    RemoteExportDesiredRecord::new(logical_key, expires_at, upsert, tombstone)
}

fn plan_session_digest_record(
    digest: &RemoteSessionDigest,
    descriptors: &BTreeMap<ObservedProjectKey, RemoteProjectDescriptor>,
) -> io::Result<RemoteExportDesiredRecord> {
    let logical_key = format!(
        "session-digest-v1:{}:{}",
        digest.thread_id.as_str(),
        digest.range_start.timestamp()
    );
    let retention_through = digest
        .range_end
        .checked_add_days(Days::new(SESSION_DIGEST_RETENTION_DAYS))
        .ok_or_else(|| invalid_data("remote session digest retention bound overflows"))?;
    let referenced = digest
        .observed_project_keys
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    let upsert = encode_remote_delta_journal_record(
        RemoteDeltaJournalRecord::SessionDigest {
            thread_id: digest.thread_id.clone(),
            range_start: digest.range_start,
            range_end: digest.range_end,
            changed_at: digest.covered_through,
            retention_through,
            mutation: RemoteSessionDigestMutation::Upsert(Box::new(digest.clone())),
        },
        descriptors_for_keys(&referenced, descriptors)?,
    )?;
    let tombstone = encode_remote_delta_journal_record(
        RemoteDeltaJournalRecord::SessionDigest {
            thread_id: digest.thread_id.clone(),
            range_start: digest.range_start,
            range_end: digest.range_end,
            changed_at: digest.covered_through,
            retention_through,
            mutation: RemoteSessionDigestMutation::Tombstone,
        },
        Vec::new(),
    )?;
    RemoteExportDesiredRecord::new(logical_key, retention_through, upsert, tombstone)
}

fn descriptor_index(
    descriptors: &[RemoteProjectDescriptor],
) -> io::Result<BTreeMap<ObservedProjectKey, RemoteProjectDescriptor>> {
    let mut result = BTreeMap::new();
    for descriptor in descriptors {
        match result.entry(descriptor.observed_project_key.clone()) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(descriptor.clone());
            }
            std::collections::btree_map::Entry::Occupied(entry) if entry.get() == descriptor => {}
            std::collections::btree_map::Entry::Occupied(_) => {
                return Err(invalid_data(
                    "conflicting remote project descriptors share a project key",
                ));
            }
        }
    }
    Ok(result)
}

fn descriptors_for_keys(
    keys: &BTreeSet<ObservedProjectKey>,
    descriptors: &BTreeMap<ObservedProjectKey, RemoteProjectDescriptor>,
) -> io::Result<Vec<RemoteProjectDescriptor>> {
    keys.iter()
        .map(|key| {
            descriptors.get(key).cloned().ok_or_else(|| {
                invalid_data(format!(
                    "remote aggregate references missing project descriptor {}",
                    key.as_str()
                ))
            })
        })
        .collect()
}

fn insert_unique_record(
    records: &mut BTreeMap<String, RemoteExportDesiredRecord>,
    record: RemoteExportDesiredRecord,
) -> io::Result<()> {
    if records
        .insert(record.logical_key().to_owned(), record)
        .is_some()
    {
        return Err(invalid_data(
            "remote materialized observation contains a duplicate logical key",
        ));
    }
    Ok(())
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;
    use std::str::FromStr;

    use chrono::{DateTime, Utc};

    use super::*;
    use crate::remote_protocol::{
        RemoteApiCostAmount, RemoteProjectUsageGroup, RemoteSessionDigestFingerprint,
        RemoteSessionUsageMetrics, RemoteTokenUsage, RemoteU128,
    };
    use crate::source_model::{ProjectDisplayLabel, ThreadId};

    fn at(value: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(value)
            .unwrap()
            .with_timezone(&Utc)
    }

    fn project_key(seed: u8) -> ObservedProjectKey {
        ObservedProjectKey::from_str(&format!(
            "opk-hmac-sha256-v1-{}",
            format!("{seed:02x}").repeat(32)
        ))
        .unwrap()
    }

    fn descriptor(key: ObservedProjectKey, label: &str) -> RemoteProjectDescriptor {
        RemoteProjectDescriptor {
            observed_project_key: key,
            display_label: ProjectDisplayLabel::from_str(label).unwrap(),
            git_evidence: crate::remote_protocol::RemoteGitRepositoryEvidence::Unavailable,
        }
    }

    fn token_usage(total: u64) -> RemoteTokenUsage {
        RemoteTokenUsage {
            input_tokens: total,
            total_tokens: total,
            ..RemoteTokenUsage::default()
        }
    }

    fn api_cost(total: u64) -> RemoteApiCostAmount {
        RemoteApiCostAmount {
            observed_samples: u64::from(total > 0),
            priced_samples: u64::from(total > 0),
            observed_tokens: total,
            priced_tokens: total,
            ..RemoteApiCostAmount::default()
        }
    }

    fn bucket(key: ObservedProjectKey) -> RemoteUsageBucket {
        let start = at("2026-08-30T10:00:00Z");
        RemoteUsageBucket {
            starts_at: start,
            ends_at: start + chrono::Duration::minutes(15),
            sampled_at: start + chrono::Duration::minutes(14),
            token_usage: token_usage(10),
            estimated_cost_units: RemoteU128::new(11),
            api_long_context_extra_cost_units: Some(RemoteU128::new(0)),
            long_context_usage_unknown: false,
            api_equivalent_cost: api_cost(10),
            call_count: 1,
            metric_revision: NonZeroU32::MIN,
            estimator_revision: NonZeroU32::MIN,
            project_breakdown_revision: NonZeroU32::MIN,
            api_pricing_catalog_revision: NonZeroU32::MIN,
            model_groups: Vec::new(),
            project_groups: vec![RemoteProjectUsageGroup {
                observed_project_key: Some(key),
                emitting_thread_id: ThreadId::from_str("01a00000-0000-7000-8000-000000000001")
                    .unwrap(),
                emitting_turn_id: Some("turn-1".to_owned()),
                parent_thread_id: None,
                root_session_thread_id: None,
                root_session_turn_id: None,
                title_preview: None,
                message_preview: None,
                token_usage: token_usage(10),
                estimated_cost_units: RemoteU128::new(11),
                api_long_context_extra_cost_units: Some(RemoteU128::new(0)),
                api_equivalent_cost: api_cost(10),
                call_count: 1,
            }],
            partial_reasons: Vec::new(),
        }
    }

    fn digest(key: ObservedProjectKey) -> RemoteSessionDigest {
        RemoteSessionDigest {
            thread_id: ThreadId::from_str("01a00000-0000-7000-8000-000000000001").unwrap(),
            range_start: at("2026-08-30T00:00:00Z"),
            range_end: at("2026-08-31T00:00:00Z"),
            covered_through: at("2026-08-30T12:00:00Z"),
            fingerprint: RemoteSessionDigestFingerprint::from_str(&format!(
                "session-digest-sha256-v1-{}",
                "12".repeat(32)
            ))
            .unwrap(),
            project_breakdown_fingerprint: RemoteSessionDigestFingerprint::from_str(&format!(
                "session-digest-sha256-v1-{}",
                "13".repeat(32)
            ))
            .unwrap(),
            event_count: 1,
            exact_event_identity: true,
            coverage_complete: false,
            observed_project_keys: vec![key],
            metrics: RemoteSessionUsageMetrics {
                token_usage: token_usage(10),
                estimated_cost_units: RemoteU128::new(11),
                api_long_context_extra_cost_units: Some(RemoteU128::new(0)),
                api_equivalent_cost: api_cost(10),
                call_count: 1,
                metric_revision: NonZeroU32::MIN,
                estimator_revision: NonZeroU32::MIN,
                project_breakdown_revision: NonZeroU32::MIN,
                api_pricing_catalog_revision: NonZeroU32::MIN,
                partial_reasons: vec!["session_range_open".to_owned()],
            },
        }
    }

    #[test]
    fn partial_export_records_preserve_components_groups_and_coverage() {
        fn bucket_record(value: RemoteUsageBucket) -> RemoteDeltaJournalRecord {
            RemoteDeltaJournalRecord::UsageBucket {
                starts_at: value.starts_at,
                mutation: RemoteUsageBucketMutation::Upsert(Box::new(value)),
            }
        }
        let mut old = bucket(project_key(0x11));
        old.partial_reasons.push("rollout_scan_incomplete".into());
        let old_record = bucket_record(old.clone());
        assert!(partial::record_at_least(&old_record, &old_record));
        let mut larger = old.clone();
        larger.token_usage.input_tokens += 10;
        larger.token_usage.total_tokens += 10;
        larger.project_groups[0].token_usage = larger.token_usage;
        assert!(partial::record_at_least(
            &bucket_record(larger.clone()),
            &old_record
        ));
        for mutate in [
            |value: &mut RemoteUsageBucket| value.project_groups.clear(),
            |value: &mut RemoteUsageBucket| value.project_groups[0].token_usage = token_usage(1),
            |value: &mut RemoteUsageBucket| value.api_equivalent_cost.priced_tokens = 0,
            |value: &mut RemoteUsageBucket| value.api_long_context_extra_cost_units = None,
            |value: &mut RemoteUsageBucket| {
                value.project_groups[0].root_session_turn_id = Some("other".into())
            },
        ] {
            let mut next = larger.clone();
            mutate(&mut next);
            assert!(!partial::record_at_least(&bucket_record(next), &old_record));
        }
        old.partial_reasons.clear();
        assert!(!partial::record_at_least(
            &bucket_record(larger),
            &bucket_record(old)
        ));

        let old = digest(project_key(0x11));
        let record = |value: RemoteSessionDigest| RemoteDeltaJournalRecord::SessionDigest {
            thread_id: value.thread_id.clone(),
            range_start: value.range_start,
            range_end: value.range_end,
            changed_at: value.covered_through,
            retention_through: value.range_end + chrono::Duration::days(35),
            mutation: RemoteSessionDigestMutation::Upsert(Box::new(value)),
        };
        assert!(partial::record_at_least(
            &record(old.clone()),
            &record(old.clone())
        ));
        let mut less = old.clone();
        less.exact_event_identity = false;
        assert!(!partial::record_at_least(
            &record(less),
            &record(old.clone())
        ));
        let mut complete = old.clone();
        complete.coverage_complete = true;
        complete.covered_through = complete.range_end;
        assert!(!partial::record_at_least(&record(old), &record(complete)));
    }

    #[test]
    fn plan_is_sorted_stable_and_prepares_distinct_tombstones() {
        let key = project_key(0x11);
        let materialized = MaterializedSourceObservation {
            project_descriptors: vec![descriptor(key.clone(), "workspace")],
            buckets: vec![bucket(key.clone())],
            session_digests: vec![digest(key)],
            ..MaterializedSourceObservation::default()
        };

        let first = plan_remote_export_records(&materialized).unwrap();
        let second = plan_remote_export_records(&materialized).unwrap();

        assert_eq!(first, second);
        assert_eq!(first.len(), 2);
        assert!(first[0].logical_key() < first[1].logical_key());
        assert!(
            first
                .iter()
                .all(|record| record.upsert().change_id() != record.tombstone().change_id())
        );
        let planned_bucket = first
            .iter()
            .find(|record| record.logical_key().starts_with("bucket-v1:"))
            .unwrap();
        assert_eq!(planned_bucket.expires_at(), at("2026-10-04T10:15:00Z"));
        let planned_digest = first
            .iter()
            .find(|record| record.logical_key().starts_with("session-digest-v1:"))
            .unwrap();
        assert_eq!(planned_digest.expires_at(), at("2026-10-05T00:00:00Z"));
    }

    #[test]
    fn plan_rejects_missing_conflicting_and_duplicate_identity_inputs() {
        let key = project_key(0x22);
        let missing = MaterializedSourceObservation {
            buckets: vec![bucket(key.clone())],
            ..MaterializedSourceObservation::default()
        };
        assert!(plan_remote_export_records(&missing).is_err());

        let conflicting = MaterializedSourceObservation {
            project_descriptors: vec![descriptor(key.clone(), "a"), descriptor(key.clone(), "b")],
            ..MaterializedSourceObservation::default()
        };
        assert!(plan_remote_export_records(&conflicting).is_err());

        let duplicate = MaterializedSourceObservation {
            project_descriptors: vec![descriptor(key.clone(), "a")],
            buckets: vec![bucket(key.clone()), bucket(key)],
            ..MaterializedSourceObservation::default()
        };
        assert!(plan_remote_export_records(&duplicate).is_err());
    }

    #[test]
    fn session_retention_is_stable_and_checked() {
        let key = project_key(0x33);
        let mut session = digest(key.clone());
        let materialized = MaterializedSourceObservation {
            project_descriptors: vec![descriptor(key, "workspace")],
            session_digests: vec![session.clone()],
            ..MaterializedSourceObservation::default()
        };
        let first = plan_remote_export_records(&materialized).unwrap();
        let second = plan_remote_export_records(&materialized).unwrap();
        assert_eq!(first[0].upsert(), second[0].upsert());

        session.range_end = DateTime::<Utc>::MAX_UTC;
        session.covered_through = session.range_end;
        let overflowing = MaterializedSourceObservation {
            session_digests: vec![session],
            ..MaterializedSourceObservation::default()
        };
        assert!(plan_remote_export_records(&overflowing).is_err());

        let bucket_key = project_key(0x44);
        let mut overflowing_bucket = bucket(bucket_key.clone());
        overflowing_bucket.ends_at = DateTime::<Utc>::MAX_UTC;
        let overflowing = MaterializedSourceObservation {
            project_descriptors: vec![descriptor(bucket_key, "workspace")],
            buckets: vec![overflowing_bucket],
            ..MaterializedSourceObservation::default()
        };
        assert!(plan_remote_export_records(&overflowing).is_err());
    }
}
