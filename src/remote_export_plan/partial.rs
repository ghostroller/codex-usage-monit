//! Partial scans can add evidence but cannot lower any previously published
//! token, price, group or coverage component. Missing keys remain untouched.
use std::collections::BTreeMap;
use std::io;

use crate::remote_delta_journal::{RemoteDeltaJournalRecord, validated_journal_record};
use crate::remote_export_state::{RemoteExportDesiredRecord, RemoteExportMaterializedUpsert};
use crate::remote_protocol::{
    RemoteApiCostAmount, RemoteSessionDigestMutation, RemoteTokenUsage, RemoteUsageBucketMutation,
};

macro_rules! metrics_at_least {
    ($next:expr, $old:expr) => {
        tokens_at_least($next.token_usage, $old.token_usage)
            && $next.call_count >= $old.call_count
            && $next.estimated_cost_units >= $old.estimated_cost_units
            && $next.api_long_context_extra_cost_units >= $old.api_long_context_extra_cost_units
            && api_at_least($next.api_equivalent_cost, $old.api_equivalent_cost)
    };
}

macro_rules! same_revisions {
    ($next:expr, $old:expr) => {
        $next.metric_revision == $old.metric_revision
            && $next.estimator_revision == $old.estimator_revision
            && $next.project_breakdown_revision == $old.project_breakdown_revision
            && $next.api_pricing_catalog_revision == $old.api_pricing_catalog_revision
    };
}

pub(crate) fn filter_partial_export_records(
    desired: Vec<RemoteExportDesiredRecord>,
    previous: &[RemoteExportMaterializedUpsert],
) -> io::Result<Vec<RemoteExportDesiredRecord>> {
    let previous = previous
        .iter()
        .map(|value| (value.logical_key(), value))
        .collect::<BTreeMap<_, _>>();
    let mut accepted = Vec::with_capacity(desired.len());
    for candidate in desired {
        let Some(old) = previous.get(candidate.logical_key()) else {
            accepted.push(candidate);
            continue;
        };
        if candidate.expires_at() >= old.expires_at()
            && record_at_least(
                &validated_journal_record(candidate.upsert())?,
                &validated_journal_record(old.change())?,
            )
        {
            accepted.push(candidate);
        }
    }
    Ok(accepted)
}

pub(super) fn record_at_least(
    next: &RemoteDeltaJournalRecord,
    old: &RemoteDeltaJournalRecord,
) -> bool {
    match (next, old) {
        (
            RemoteDeltaJournalRecord::UsageBucket {
                mutation: RemoteUsageBucketMutation::Upsert(next),
                ..
            },
            RemoteDeltaJournalRecord::UsageBucket {
                mutation: RemoteUsageBucketMutation::Upsert(old),
                ..
            },
        ) => {
            let models = next
                .model_groups
                .iter()
                .map(|group| ((&group.model, &group.service_tier), group))
                .collect::<BTreeMap<_, _>>();
            let projects = next
                .project_groups
                .iter()
                .map(|group| (project_key(group), group))
                .collect::<BTreeMap<_, _>>();
            next.starts_at == old.starts_at
                && next.ends_at == old.ends_at
                && next.sampled_at >= old.sampled_at
                && same_revisions!(next, old)
                && metrics_at_least!(next, old)
                && (!next.long_context_usage_unknown || old.long_context_usage_unknown)
                && preserves_scan_quality(&next.partial_reasons, &old.partial_reasons)
                && old.model_groups.iter().all(|old| {
                    models
                        .get(&(&old.model, &old.service_tier))
                        .is_some_and(|next| {
                            metrics_at_least!(next, old)
                                && (!next.used_model_fallback || old.used_model_fallback)
                                && (!next.used_token_breakdown_fallback
                                    || old.used_token_breakdown_fallback)
                                && (!next.used_long_context_detection_fallback
                                    || old.used_long_context_detection_fallback)
                        })
                })
                && old.project_groups.iter().all(|old| {
                    projects
                        .get(&project_key(old))
                        .is_some_and(|next| metrics_at_least!(next, old))
                })
        }
        (
            RemoteDeltaJournalRecord::SessionDigest {
                mutation: RemoteSessionDigestMutation::Upsert(next),
                ..
            },
            RemoteDeltaJournalRecord::SessionDigest {
                mutation: RemoteSessionDigestMutation::Upsert(old),
                ..
            },
        ) => {
            next.thread_id == old.thread_id
                && next.range_start == old.range_start
                && next.range_end == old.range_end
                && next.covered_through >= old.covered_through
                && next.event_count >= old.event_count
                && (!old.exact_event_identity || next.exact_event_identity)
                && (!old.coverage_complete || next.coverage_complete)
                && old
                    .observed_project_keys
                    .iter()
                    .all(|key| next.observed_project_keys.binary_search(key).is_ok())
                && same_revisions!(next.metrics, old.metrics)
                && metrics_at_least!(next.metrics, old.metrics)
                && preserves_scan_quality(
                    &next.metrics.partial_reasons,
                    &old.metrics.partial_reasons,
                )
        }
        _ => false,
    }
}

fn preserves_scan_quality(next: &[String], old: &[String]) -> bool {
    !next
        .iter()
        .any(|reason| reason == "rollout_scan_incomplete")
        || old.iter().any(|reason| reason == "rollout_scan_incomplete")
}

fn project_key(group: &crate::remote_protocol::RemoteProjectUsageGroup) -> impl Ord + '_ {
    (
        &group.observed_project_key,
        &group.emitting_thread_id,
        &group.emitting_turn_id,
        &group.parent_thread_id,
        &group.root_session_thread_id,
        &group.root_session_turn_id,
    )
}

fn tokens_at_least(next: RemoteTokenUsage, old: RemoteTokenUsage) -> bool {
    next.total_tokens >= old.total_tokens
        && next.input_tokens >= old.input_tokens
        && next.output_tokens >= old.output_tokens
        && next.cached_input_tokens >= old.cached_input_tokens
        && next.cache_write_input_tokens >= old.cache_write_input_tokens
        && next.reasoning_output_tokens >= old.reasoning_output_tokens
        && next.unclassified_tokens >= old.unclassified_tokens
}

fn api_at_least(next: RemoteApiCostAmount, old: RemoteApiCostAmount) -> bool {
    next.minimum_pico_usd >= old.minimum_pico_usd
        && next.maximum_pico_usd >= old.maximum_pico_usd
        && next.observed_samples >= old.observed_samples
        && next.priced_samples >= old.priced_samples
        && next.observed_tokens >= old.observed_tokens
        && next.priced_tokens >= old.priced_tokens
}
