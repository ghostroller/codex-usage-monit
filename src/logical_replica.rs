//! Pure detection of physical session replicas across included sources.
//!
//! This module deliberately performs no storage or network I/O. Query readers
//! and background fact schedulers feed it the source-filtered digest rows they
//! already own, so both paths agree on which physical threads need exact facts.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};

use crate::domain::{ApiCostAmount, TokenUsage};
use crate::source_history::{
    ActiveFactSet, SourceHistoryRemoteBinding, SourceSessionDigest, UsageEventFact,
};
use crate::source_identity::NodeId;
use crate::source_model::ThreadId;

#[derive(Clone, Copy, Debug)]
pub(crate) struct ReplicaDigestObservation<'a> {
    pub source_id: &'a NodeId,
    pub digest: &'a SourceSessionDigest,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReplicaCandidateKind {
    /// Every replica carries the same complete, exact, revision-compatible
    /// digest. A deterministic authority can be selected without event facts.
    Identical,
    /// The replicas disagree or cannot prove exact complete equality. Exact
    /// facts are required before a union can be attempted.
    NeedsFacts,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum ExpectedReplicaFactBinding<'a> {
    Local,
    Remote(&'a SourceHistoryRemoteBinding),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ReplicaCandidate {
    thread_id: ThreadId,
    range_start: DateTime<Utc>,
    range_end: DateTime<Utc>,
    source_ids: Vec<NodeId>,
    kind: ReplicaCandidateKind,
}

impl ReplicaCandidate {
    pub fn thread_id(&self) -> &ThreadId {
        &self.thread_id
    }

    pub fn range_start(&self) -> DateTime<Utc> {
        self.range_start
    }

    pub fn range_end(&self) -> DateTime<Utc> {
        self.range_end
    }

    pub fn source_ids(&self) -> &[NodeId] {
        &self.source_ids
    }

    pub fn kind(&self) -> ReplicaCandidateKind {
        self.kind
    }
}

/// Finds same-thread/day replicas after source selection has already run.
///
/// A candidate key is `(thread_id, range_start)`, matching the durable digest
/// key. Multiple rows from one source never manufacture a cross-source
/// candidate. Output and source order are independent of arrival order.
pub(crate) fn detect_replica_candidates<'a>(
    observations: impl IntoIterator<Item = ReplicaDigestObservation<'a>>,
) -> Vec<ReplicaCandidate> {
    let mut by_key = BTreeMap::<
        (ThreadId, DateTime<Utc>),
        BTreeMap<String, (&NodeId, &SourceSessionDigest)>,
    >::new();
    for observation in observations {
        let digest = observation.digest;
        by_key
            .entry((digest.replica().thread_id().clone(), digest.range_start()))
            .or_default()
            .entry(observation.source_id.as_str().to_owned())
            .or_insert((observation.source_id, digest));
    }

    by_key
        .into_iter()
        .filter_map(|((thread_id, range_start), sources)| {
            if sources.len() < 2 {
                return None;
            }
            let mut digests = sources.values().map(|(_, digest)| *digest);
            let first = digests.next().expect("two-source candidate has a digest");
            let identical = digest_proves_identical(first)
                && digests.all(|digest| {
                    digest_proves_identical(digest) && identical_digest(first, digest)
                });
            let range_end = sources
                .values()
                .map(|(_, digest)| digest.range_end())
                .max()
                .expect("two-source candidate has a range");
            Some(ReplicaCandidate {
                thread_id,
                range_start,
                range_end,
                source_ids: sources
                    .into_values()
                    .map(|(source_id, _)| source_id.clone())
                    .collect(),
                kind: if identical {
                    ReplicaCandidateKind::Identical
                } else {
                    ReplicaCandidateKind::NeedsFacts
                },
            })
        })
        .collect()
}

/// Proves that one active fact generation is a complete, exact materialization
/// of the candidate digest range.
///
/// `Remote` requires the fact manifest's exact exporter generation/revisions
/// to equal the active remote history generation captured by the caller.
/// `Local` requires the persisted fact set to have no remote binding. The
/// function is pure and does not load, refresh, or schedule facts.
pub(crate) fn active_facts_cover_digest(
    digest: &SourceSessionDigest,
    active: Option<&ActiveFactSet>,
    expected_binding: ExpectedReplicaFactBinding<'_>,
) -> bool {
    if !digest.exact_event_identity() {
        return false;
    }
    let Some(active) = active else {
        return false;
    };
    let Some(validated_binding) = active
        .version
        .validated_digests()
        .iter()
        .find(|binding| binding.matches_digest(digest))
    else {
        return false;
    };
    let binding_matches = match expected_binding {
        ExpectedReplicaFactBinding::Local => active.remote_binding.is_none(),
        ExpectedReplicaFactBinding::Remote(expected) => {
            let revisions = expected.revisions();
            active.remote_binding.as_ref() == Some(expected)
                && digest.metrics().metric_revision == revisions.metric.get()
                && digest.metrics().estimator_revision == revisions.estimator.get()
                && digest.metrics().project_breakdown_revision == revisions.project_breakdown.get()
                && digest.metrics().api_pricing_catalog_revision
                    == revisions.api_pricing_catalog.get()
        }
    };
    if !binding_matches
        || active
            .version
            .retained_since()
            .is_some_and(|retained_since| retained_since > digest.range_start())
    {
        return false;
    }

    let facts = active
        .facts()
        .into_iter()
        .filter(|fact| {
            fact.occurred_at() >= digest.range_start() && fact.occurred_at() < digest.range_end()
        })
        .collect::<Vec<_>>();
    facts.len() == usize::try_from(digest.event_count()).unwrap_or(usize::MAX)
        && facts.iter().all(|fact| {
            // A normalized cumulative-counter delta can have stable event
            // identity even when it cannot prove that the delta represents
            // exactly one API request. That uncertainty is retained in the
            // fact's partial reasons and cost bounds; it must not make an
            // otherwise exact replica union permanently unreachable.
            fact.exact_event_identity()
                && fact.replica().thread_id() == digest.replica().thread_id()
        })
        && crate::source_export::validate_fact_digest_bindings_against_facts(
            digest.replica(),
            &facts,
            std::slice::from_ref(validated_binding),
            active.version.retained_since(),
        )
        .is_ok()
        && fact_metrics_match_digest(&facts, digest)
}

fn fact_metrics_match_digest(facts: &[&UsageEventFact], digest: &SourceSessionDigest) -> bool {
    let expected = digest.metrics();
    let mut token_usage = TokenUsage::default();
    let mut estimated_cost_units = 0_u128;
    let mut long_context = Some(0_u128);
    let mut api_cost = ApiCostAmount::default();
    let mut call_count = 0_u64;
    for fact in facts {
        let metrics = fact.metrics();
        if metrics.metric_revision != expected.metric_revision
            || metrics.estimator_revision != expected.estimator_revision
            || metrics.project_breakdown_revision != expected.project_breakdown_revision
            || metrics.api_pricing_catalog_revision != expected.api_pricing_catalog_revision
        {
            return false;
        }
        token_usage.add_assign(metrics.token_usage);
        estimated_cost_units = estimated_cost_units.saturating_add(metrics.estimated_cost_units);
        long_context = match (long_context, metrics.api_long_context_extra_cost_units) {
            (Some(left), Some(right)) => Some(left.saturating_add(right)),
            _ => None,
        };
        api_cost.add_assign(metrics.api_equivalent_cost);
        call_count = call_count.saturating_add(metrics.call_count);
    }
    token_usage == expected.token_usage
        && estimated_cost_units == expected.estimated_cost_units
        && long_context == expected.api_long_context_extra_cost_units
        && api_cost == expected.api_equivalent_cost
        && call_count == expected.call_count
}

fn digest_proves_identical(digest: &SourceSessionDigest) -> bool {
    digest.exact_event_identity() && digest.coverage_complete()
}

fn identical_digest(left: &SourceSessionDigest, right: &SourceSessionDigest) -> bool {
    left.range_start() == right.range_start()
        && left.range_end() == right.range_end()
        && left.covered_through() == right.covered_through()
        && left.fingerprint() == right.fingerprint()
        && left.project_breakdown_fingerprint() == right.project_breakdown_fingerprint()
        && left.event_count() == right.event_count()
        && left.metrics() == right.metrics()
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU64;
    use std::str::FromStr;

    use chrono::{Duration, TimeZone};

    use super::*;
    use crate::remote_protocol::SourceGeneration;
    use crate::source_history::{
        CompleteFactBatch, FactBatchId, FactBatchKind, FactCursor, RedactionProfile,
        SessionDigestFingerprint, SessionUsageMetrics, SourceHistoryStore, SourceKind,
        SourceMetadata, UsageEventFactRecord, UsageEventId,
    };
    use crate::source_model::SessionReplicaKey;

    const SOURCE_A: &str = "node-0123456789abcdef0123456789abcdef";
    const SOURCE_B: &str = "node-fedcba9876543210fedcba9876543210";

    fn at(hour: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 30, hour, 0, 0)
            .single()
            .unwrap()
    }

    fn digest(source: &NodeId, fingerprint: char) -> SourceSessionDigest {
        digest_with_project(source, fingerprint, fingerprint)
    }

    fn digest_with_project(
        source: &NodeId,
        fingerprint: char,
        project_fingerprint: char,
    ) -> SourceSessionDigest {
        let revisions = crate::remote_agent::current_revisions();
        SourceSessionDigest::new(
            SessionReplicaKey::new(source.clone(), "thread-a".parse().unwrap()),
            at(0),
            at(0) + Duration::days(1),
            at(0) + Duration::days(1),
            SessionDigestFingerprint::from_str(&format!(
                "session-digest-sha256-v1-{}",
                fingerprint.to_string().repeat(64)
            ))
            .unwrap(),
            SessionDigestFingerprint::from_str(&format!(
                "session-digest-sha256-v1-{}",
                project_fingerprint.to_string().repeat(64)
            ))
            .unwrap(),
            1,
            true,
            true,
            Vec::new(),
            SessionUsageMetrics {
                token_usage: TokenUsage {
                    input_tokens: 7,
                    total_tokens: 7,
                    ..TokenUsage::default()
                },
                estimated_cost_units: 7,
                api_long_context_extra_cost_units: Some(0),
                call_count: 1,
                metric_revision: revisions.metric.get(),
                estimator_revision: revisions.estimator.get(),
                project_breakdown_revision: revisions.project_breakdown.get(),
                api_pricing_catalog_revision: revisions.api_pricing_catalog.get(),
                ..SessionUsageMetrics::default()
            },
        )
        .unwrap()
    }

    #[test]
    fn detection_is_source_filtered_and_arrival_order_independent() {
        let a: NodeId = SOURCE_A.parse().unwrap();
        let b: NodeId = SOURCE_B.parse().unwrap();
        let digest_a = digest(&a, 'a');
        let digest_b = digest(&b, 'a');
        let forward = detect_replica_candidates([
            ReplicaDigestObservation {
                source_id: &a,
                digest: &digest_a,
            },
            ReplicaDigestObservation {
                source_id: &b,
                digest: &digest_b,
            },
        ]);
        let reverse = detect_replica_candidates([
            ReplicaDigestObservation {
                source_id: &b,
                digest: &digest_b,
            },
            ReplicaDigestObservation {
                source_id: &a,
                digest: &digest_a,
            },
        ]);
        assert_eq!(forward, reverse);
        assert_eq!(forward[0].kind(), ReplicaCandidateKind::Identical);
        assert_eq!(forward[0].source_ids(), &[a.clone(), b.clone()]);
        assert!(
            detect_replica_candidates([ReplicaDigestObservation {
                source_id: &a,
                digest: &digest_a,
            }])
            .is_empty()
        );
    }

    #[test]
    fn divergent_or_inexact_digest_requires_facts() {
        let a: NodeId = SOURCE_A.parse().unwrap();
        let b: NodeId = SOURCE_B.parse().unwrap();
        let digest_a = digest(&a, 'a');
        let digest_b = digest(&b, 'b');
        let result = detect_replica_candidates([
            ReplicaDigestObservation {
                source_id: &a,
                digest: &digest_a,
            },
            ReplicaDigestObservation {
                source_id: &b,
                digest: &digest_b,
            },
        ]);
        assert_eq!(result[0].kind(), ReplicaCandidateKind::NeedsFacts);
    }

    #[test]
    fn fact_coverage_requires_exact_local_or_remote_generation_binding() {
        let directory = tempfile::tempdir().unwrap();
        let source: NodeId = SOURCE_A.parse().unwrap();
        let store = SourceHistoryStore::new(
            directory.path().join("state"),
            "0123456789abcdef".parse().unwrap(),
        );
        store
            .save_source_metadata(
                &SourceMetadata::new(source.clone(), SourceKind::Ssh, "remote").unwrap(),
            )
            .unwrap();
        let baseline_metrics = digest(&source, 'a').metrics().clone();
        let replica = SessionReplicaKey::new(source.clone(), "thread-a".parse().unwrap());
        let fact = UsageEventFact::new(
            replica.clone(),
            UsageEventId::from_str("event-a").unwrap(),
            at(1),
            "opk-hmac-sha256-v1-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                .parse()
                .unwrap(),
            Some("turn-a".to_string()),
            None,
            Some("thread-a".parse().unwrap()),
            "thread-a".parse().unwrap(),
            Some("turn-a".to_string()),
            Some("gpt-test".to_string()),
            None,
            baseline_metrics.token_usage,
            false,
            true,
            baseline_metrics.clone(),
        )
        .unwrap();
        let (fingerprint, project_fingerprint) =
            crate::source_export::canonical_fact_fingerprints_for_test(
                &replica,
                at(0),
                at(0) + Duration::days(1),
                &[&fact],
            )
            .unwrap();
        let baseline_digest = SourceSessionDigest::new(
            replica.clone(),
            at(0),
            at(0) + Duration::days(1),
            at(0) + Duration::days(1),
            fingerprint,
            project_fingerprint,
            1,
            true,
            true,
            vec![fact.observed_project_key().clone()],
            baseline_metrics,
        )
        .unwrap();
        let binding = SourceHistoryRemoteBinding::new(
            SourceGeneration {
                node_id: source.clone(),
                generation: NonZeroU64::new(7).unwrap(),
            },
            crate::remote_agent::current_revisions(),
        )
        .unwrap();
        let batch = CompleteFactBatch {
            batch_id: FactBatchId::generate().unwrap(),
            kind: FactBatchKind::Snapshot,
            replica: replica.clone(),
            expected_active_version: None,
            remote_binding: Some(binding.clone()),
            validated_digests: vec![
                crate::source_history::FactDigestBinding::from_digest(&baseline_digest).unwrap(),
            ],
            activate_cursor: FactCursor::new(1, 1).unwrap(),
            completed_at: at(2),
            changes: vec![UsageEventFactRecord::upsert(1, fact).unwrap()],
        };
        store
            .stage_complete_fact_batch(&source, RedactionProfile::Redacted, &batch)
            .unwrap();
        store
            .activate_staged_fact_batch(&source, RedactionProfile::Redacted, &batch.batch_id)
            .unwrap();
        let active = store
            .load_active_fact_set(
                &source,
                RedactionProfile::Redacted,
                &"thread-a".parse().unwrap(),
            )
            .unwrap()
            .unwrap();
        assert!(
            active
                .facts()
                .iter()
                .all(|fact| !fact.request_usage_exact())
        );
        assert!(active_facts_cover_digest(
            &baseline_digest,
            Some(&active),
            ExpectedReplicaFactBinding::Remote(&binding),
        ));
        let forged_project_fact = UsageEventFact::new(
            replica,
            UsageEventId::from_str("event-a").unwrap(),
            at(1),
            "opk-hmac-sha256-v1-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                .parse()
                .unwrap(),
            Some("turn-a".to_string()),
            None,
            Some("thread-a".parse().unwrap()),
            "thread-a".parse().unwrap(),
            Some("turn-a".to_string()),
            Some("gpt-test".to_string()),
            None,
            baseline_digest.metrics().token_usage,
            false,
            true,
            baseline_digest.metrics().clone(),
        )
        .unwrap();
        let mut forged_active = active.clone();
        forged_active.records = vec![UsageEventFactRecord::upsert(1, forged_project_fact).unwrap()];
        assert!(!active_facts_cover_digest(
            &baseline_digest,
            Some(&forged_active),
            ExpectedReplicaFactBinding::Remote(&binding),
        ));
        assert!(!active_facts_cover_digest(
            &baseline_digest,
            Some(&active),
            ExpectedReplicaFactBinding::Local,
        ));
        let next_binding = SourceHistoryRemoteBinding::new(
            SourceGeneration {
                node_id: source,
                generation: NonZeroU64::new(8).unwrap(),
            },
            crate::remote_agent::current_revisions(),
        )
        .unwrap();
        assert!(!active_facts_cover_digest(
            &baseline_digest,
            Some(&active),
            ExpectedReplicaFactBinding::Remote(&next_binding),
        ));
        let changed_event_identity = digest(&SOURCE_A.parse().unwrap(), 'b');
        assert!(!active_facts_cover_digest(
            &changed_event_identity,
            Some(&active),
            ExpectedReplicaFactBinding::Remote(&binding),
        ));
        let changed_project_breakdown = digest_with_project(&SOURCE_A.parse().unwrap(), 'a', 'c');
        assert!(!active_facts_cover_digest(
            &changed_project_breakdown,
            Some(&active),
            ExpectedReplicaFactBinding::Remote(&binding),
        ));
    }
}
