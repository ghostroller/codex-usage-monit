//! Bucket and weekly aggregation projections for source-aware history reads.

use std::collections::{BTreeMap, BTreeSet};
use std::io;

use chrono::{DateTime, Duration, Utc};

use super::reconciliation::{LogicalThreadProjection, add_optional_units};
use super::{
    DUPLICATE_SESSION_WEEKLY_REBUILT_FROM_BUCKETS, RESET_DRIFT_SECONDS, SourceSlice,
    WEEKLY_WINDOW_MINUTES,
};
use crate::domain::TokenUsage;
use crate::history::{
    LocalHalfHourBucket, LocalProjectUsageGroup, LocalUsageGroup, QuotaPoint, WeeklyLocalPoint,
};
use crate::project_mapping::ProjectMappingProjection;
use crate::source_history::{SourceKind, SourceMetadata};
use crate::source_identity::NodeId;
use crate::source_model::ObservedProjectKey;
use crate::trace::{TraceFields, TraceOutcome, process_trace_log};

// Source history retains 35 days. A weekly reset can therefore contribute at
// most five complete cycles plus boundary/current-cycle corrections. Sixteen
// leaves ample room for clock-drift anchors while bounding corrupt remote
// inputs before cycle projection multiplies their work.
pub(super) const MAX_WEEKLY_RESET_CYCLES: usize = 16;
const MAX_WEEKLY_PROJECTION_MEMBERSHIPS: usize = 4_000_000;

pub(super) struct BucketProjection {
    pub(super) buckets: Vec<LocalHalfHourBucket>,
    pub(super) project_observations: bool,
    pub(super) unmapped_projects: bool,
}

#[cfg(test)]
pub(super) fn aggregate_source_buckets(
    sources: &[SourceSlice],
    project_mapping: &ProjectMappingProjection,
) -> BucketProjection {
    aggregate_source_buckets_with_logical_threads(
        sources,
        project_mapping,
        &LogicalThreadProjection::default(),
    )
}

pub(super) fn aggregate_source_buckets_with_logical_threads(
    sources: &[SourceSlice],
    project_mapping: &ProjectMappingProjection,
    logical_threads: &LogicalThreadProjection,
) -> BucketProjection {
    let mut buckets = BTreeMap::<DateTime<Utc>, LocalHalfHourBucket>::new();
    let mut project_observations = false;
    let mut unmapped_projects = false;
    for source in sources {
        for bucket in &source.buckets {
            let mut incoming = bucket.clone();
            let scoped = scope_project_groups(
                &mut incoming.project_groups,
                &source.metadata,
                project_mapping,
                logical_threads,
            );
            project_observations |= scoped.project_observations;
            unmapped_projects |= scoped.unmapped_projects;
            match buckets.entry(incoming.starts_at) {
                std::collections::btree_map::Entry::Vacant(entry) => {
                    entry.insert(incoming);
                }
                std::collections::btree_map::Entry::Occupied(mut entry) => {
                    merge_additive_bucket(entry.get_mut(), incoming);
                }
            }
        }
    }
    BucketProjection {
        buckets: buckets.into_values().collect(),
        project_observations,
        unmapped_projects,
    }
}

fn merge_additive_bucket(target: &mut LocalHalfHourBucket, mut incoming: LocalHalfHourBucket) {
    reconcile_estimator_revision(target, &mut incoming);
    reconcile_api_catalog_revision(target, &mut incoming);
    target.ends_at = target.ends_at.min(incoming.ends_at);
    // The aggregate is closed only when every contributing source is closed.
    target.sampled_at = target.sampled_at.min(incoming.sampled_at);
    target.token_usage.add_assign(incoming.token_usage);
    target.estimated_cost_units = target
        .estimated_cost_units
        .saturating_add(incoming.estimated_cost_units);
    target.api_long_context_extra_cost_units = add_optional_units(
        target.api_long_context_extra_cost_units,
        incoming.api_long_context_extra_cost_units,
    );
    target.long_context_usage_unknown |= incoming.long_context_usage_unknown;
    target.project_breakdown_revision = target
        .project_breakdown_revision
        .min(incoming.project_breakdown_revision);
    target.call_count = target.call_count.saturating_add(incoming.call_count);
    merge_usage_groups(&mut target.groups, incoming.groups);
    target.project_groups.extend(incoming.project_groups);
    target.partial_reasons.extend(incoming.partial_reasons);
    target.partial_reasons.sort();
    target.partial_reasons.dedup();
    target.project_groups.sort_by(|left, right| {
        left.project_id
            .cmp(&right.project_id)
            .then_with(|| left.thread_id.cmp(&right.thread_id))
            .then_with(|| left.turn_id.cmp(&right.turn_id))
    });
}

fn reconcile_estimator_revision(
    target: &mut LocalHalfHourBucket,
    incoming: &mut LocalHalfHourBucket,
) {
    if target.estimator_revision == incoming.estimator_revision {
        return;
    }

    let current = crate::history::current_history_estimator_revision();
    let target_is_current = target.estimator_revision == current;
    let incoming_is_current = incoming.estimator_revision == current;
    if !target_is_current {
        mask_bucket_estimator_projection(target);
    }
    if !incoming_is_current {
        mask_bucket_estimator_projection(incoming);
    }
    // When one contributor is current, the remaining non-zero values use the
    // active estimator. If neither is current, zero is an explicit sentinel;
    // downstream readers will keep the raw token evidence but reject all
    // derived values for this mixed slot.
    target.estimator_revision = if target_is_current || incoming_is_current {
        current
    } else {
        0
    };
    target
        .partial_reasons
        .push("estimator_revision_changed".to_string());
}

fn mask_bucket_estimator_projection(bucket: &mut LocalHalfHourBucket) {
    if !bucket.token_usage.is_zero()
        || bucket.call_count > 0
        || bucket.estimated_cost_units > 0
        || bucket
            .api_long_context_extra_cost_units
            .is_some_and(|value| value > 0)
    {
        bucket.long_context_usage_unknown = true;
    }
    bucket.estimated_cost_units = 0;
    bucket.api_long_context_extra_cost_units = Some(0);
    for group in &mut bucket.groups {
        group.estimated_cost_units = 0;
        group.api_long_context_extra_cost_units = Some(0);
    }
    for group in &mut bucket.project_groups {
        group.estimated_cost_units = 0;
        group.api_long_context_extra_cost_units = Some(0);
    }
}

fn reconcile_api_catalog_revision(
    target: &mut LocalHalfHourBucket,
    incoming: &mut LocalHalfHourBucket,
) {
    if target.api_pricing_catalog_revision == incoming.api_pricing_catalog_revision {
        return;
    }

    let current = crate::api_cost::current_api_pricing_catalog_revision();
    let target_is_current = target.api_pricing_catalog_revision == current;
    let incoming_is_current = incoming.api_pricing_catalog_revision == current;
    if !target_is_current {
        mask_bucket_api_projection(target);
    }
    if !incoming_is_current {
        mask_bucket_api_projection(incoming);
    }
    target.api_pricing_catalog_revision = if target_is_current || incoming_is_current {
        current
    } else {
        0
    };
    target
        .partial_reasons
        .push("api_pricing_catalog_changed".to_string());
}

fn mask_bucket_api_projection(bucket: &mut LocalHalfHourBucket) {
    for group in &mut bucket.groups {
        mask_api_cost_projection(
            &mut group.api_equivalent_cost,
            group.token_usage.total_tokens,
            group.call_count,
        );
        group.api_equivalent_cost_complete = false;
    }
    for group in &mut bucket.project_groups {
        mask_api_cost_projection(
            &mut group.api_equivalent_cost,
            group.token_usage.total_tokens,
            group.call_count,
        );
    }
}

fn mask_api_cost_projection(
    amount: &mut crate::domain::ApiCostAmount,
    observed_tokens: u64,
    observed_calls: u64,
) {
    amount.minimum_pico_usd = Default::default();
    amount.maximum_pico_usd = Default::default();
    amount.observed_samples = amount.observed_samples.max(observed_calls);
    amount.priced_samples = 0;
    amount.observed_tokens = amount.observed_tokens.max(observed_tokens);
    amount.priced_tokens = 0;
}

#[cfg(test)]
mod revision_merge_tests {
    use chrono::TimeZone;

    use super::*;
    use crate::domain::{ApiCostAmount, PicoUsd};
    use crate::history::LocalProjectUsageGroup;

    fn api_amount(value: u128, tokens: u64) -> ApiCostAmount {
        ApiCostAmount {
            minimum_pico_usd: PicoUsd::new(value),
            maximum_pico_usd: PicoUsd::new(value),
            observed_samples: 1,
            priced_samples: 1,
            observed_tokens: tokens,
            priced_tokens: tokens,
        }
    }

    fn bucket(
        tokens: u64,
        estimated_cost_units: u128,
        api_cost: u128,
        estimator_revision: u32,
        api_pricing_catalog_revision: u32,
        thread_id: &str,
    ) -> LocalHalfHourBucket {
        let starts_at = Utc.with_ymd_and_hms(2026, 9, 7, 10, 0, 0).unwrap();
        let token_usage = TokenUsage {
            input_tokens: tokens,
            total_tokens: tokens,
            ..TokenUsage::default()
        };
        let usage_group = LocalUsageGroup {
            model: Some("gpt-test".to_string()),
            token_usage,
            estimated_cost_units,
            api_long_context_extra_cost_units: Some(estimated_cost_units / 2),
            call_count: 1,
            api_equivalent_cost: api_amount(api_cost, tokens),
            api_equivalent_cost_complete: true,
            ..LocalUsageGroup::default()
        };
        let project_group = LocalProjectUsageGroup {
            thread_id: thread_id.to_string(),
            token_usage,
            estimated_cost_units,
            api_long_context_extra_cost_units: Some(estimated_cost_units / 2),
            api_equivalent_cost: api_amount(api_cost, tokens),
            call_count: 1,
            ..LocalProjectUsageGroup::default()
        };
        LocalHalfHourBucket {
            starts_at,
            ends_at: starts_at + Duration::minutes(15),
            sampled_at: starts_at + Duration::minutes(15),
            token_usage,
            estimated_cost_units,
            api_long_context_extra_cost_units: Some(estimated_cost_units / 2),
            long_context_usage_unknown: false,
            estimator_revision,
            project_breakdown_revision: 1,
            api_pricing_catalog_revision,
            call_count: 1,
            groups: vec![usage_group],
            project_groups: vec![project_group],
            partial_reasons: Vec::new(),
        }
    }

    #[test]
    fn mixed_revisions_keep_only_current_derived_values_in_either_source_order() {
        let estimator = crate::history::current_history_estimator_revision();
        let api_catalog = crate::api_cost::current_api_pricing_catalog_revision();
        let current = bucket(10, 100, 1_000, estimator, api_catalog, "current");
        let outdated = bucket(
            20,
            900,
            9_000,
            estimator.saturating_add(1),
            api_catalog.saturating_add(1),
            "outdated",
        );

        for (mut target, incoming) in [
            (current.clone(), outdated.clone()),
            (outdated.clone(), current.clone()),
        ] {
            merge_additive_bucket(&mut target, incoming);
            assert_eq!(target.token_usage.total_tokens, 30);
            assert_eq!(target.estimator_revision, estimator);
            assert_eq!(target.estimated_cost_units, 100);
            assert_eq!(target.api_long_context_extra_cost_units, Some(50));
            assert!(target.long_context_usage_unknown);
            assert_eq!(target.api_pricing_catalog_revision, api_catalog);
            assert!(
                target
                    .partial_reasons
                    .iter()
                    .any(|reason| reason == "estimator_revision_changed")
            );
            assert!(
                target
                    .partial_reasons
                    .iter()
                    .any(|reason| reason == "api_pricing_catalog_changed")
            );

            assert_eq!(target.groups.len(), 1);
            let model = &target.groups[0];
            assert_eq!(model.estimated_cost_units, 100);
            assert_eq!(model.api_equivalent_cost.minimum_pico_usd.0, 1_000);
            assert_eq!(model.api_equivalent_cost.observed_tokens, 30);
            assert_eq!(model.api_equivalent_cost.priced_tokens, 10);
            assert!(!model.api_equivalent_cost_complete);

            let current_project = target
                .project_groups
                .iter()
                .find(|group| group.thread_id == "current")
                .unwrap();
            let outdated_project = target
                .project_groups
                .iter()
                .find(|group| group.thread_id == "outdated")
                .unwrap();
            assert_eq!(current_project.estimated_cost_units, 100);
            assert_eq!(
                current_project.api_equivalent_cost.minimum_pico_usd.0,
                1_000
            );
            assert_eq!(outdated_project.estimated_cost_units, 0);
            assert_eq!(outdated_project.api_equivalent_cost.minimum_pico_usd.0, 0);
            assert_eq!(outdated_project.api_equivalent_cost.priced_tokens, 0);
            assert_eq!(outdated_project.api_equivalent_cost.observed_tokens, 20);
        }
    }
}

fn merge_usage_groups(target: &mut Vec<LocalUsageGroup>, incoming: Vec<LocalUsageGroup>) {
    let mut groups = BTreeMap::<(Option<String>, Option<String>), LocalUsageGroup>::new();
    for group in target.drain(..).chain(incoming) {
        let key = (group.model.clone(), group.service_tier.clone());
        match groups.entry(key) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(group);
            }
            std::collections::btree_map::Entry::Occupied(mut entry) => {
                let existing = entry.get_mut();
                existing.token_usage.add_assign(group.token_usage);
                existing.estimated_cost_units = existing
                    .estimated_cost_units
                    .saturating_add(group.estimated_cost_units);
                existing.api_long_context_extra_cost_units = add_optional_units(
                    existing.api_long_context_extra_cost_units,
                    group.api_long_context_extra_cost_units,
                );
                existing.call_count = existing.call_count.saturating_add(group.call_count);
                existing.used_model_fallback |= group.used_model_fallback;
                existing.used_token_breakdown_fallback |= group.used_token_breakdown_fallback;
                existing.used_long_context_pricing |= group.used_long_context_pricing;
                existing.used_long_context_detection_fallback |=
                    group.used_long_context_detection_fallback;
                existing
                    .api_equivalent_cost
                    .add_assign(group.api_equivalent_cost);
                existing.api_equivalent_cost_complete &= group.api_equivalent_cost_complete;
            }
        }
    }
    *target = groups.into_values().collect();
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ProjectScopeReport {
    project_observations: bool,
    unmapped_projects: bool,
}

fn scope_project_groups(
    groups: &mut [LocalProjectUsageGroup],
    source: &SourceMetadata,
    project_mapping: &ProjectMappingProjection,
    logical_threads: &LogicalThreadProjection,
) -> ProjectScopeReport {
    let mut report = ProjectScopeReport::default();
    for group in groups {
        let logicalized = logical_identity(&group.thread_id, source.source_id(), logical_threads);
        group.thread_id = logicalized
            .clone()
            .unwrap_or_else(|| scoped_value(&group.thread_id, source.source_id()));
        group.turn_id = group
            .turn_id
            .as_deref()
            .map(|value| scoped_value(value, source.source_id()));
        group.parent_thread_id = group.parent_thread_id.as_deref().map(|value| {
            logical_identity(value, source.source_id(), logical_threads)
                .unwrap_or_else(|| scoped_value(value, source.source_id()))
        });
        group.session_thread_id = group.session_thread_id.as_deref().map(|value| {
            logical_identity(value, source.source_id(), logical_threads)
                .unwrap_or_else(|| scoped_value(value, source.source_id()))
        });
        group.session_turn_id = group
            .session_turn_id
            .as_deref()
            .map(|value| scoped_value(value, source.source_id()));
        if logicalized.is_some() {
            group.source = Some(source.display_label().to_owned());
        }
        let raw_project = group.project_id.clone();
        let raw_label = group.project_label.clone();
        let projection = raw_project
            .as_deref()
            .and_then(|value| value.parse::<ObservedProjectKey>().ok())
            .and_then(|key| project_mapping.resolve(source.source_id(), &key));
        if raw_project.is_some() {
            report.project_observations = true;
        }
        if let Some(projection) = projection {
            group.project_id = Some(projection.aggregate_id().as_str().to_owned());
            group.project_label = projection
                .display_label()
                .map(|label| label.as_str().to_owned())
                .or(raw_label);
        } else {
            report.unmapped_projects |= raw_project.is_some();
            let raw_project = raw_project.as_deref().unwrap_or("unknown");
            group.project_id = Some(scoped_value(raw_project, source.source_id()));
            let raw_label = raw_label.as_deref().unwrap_or("unknown");
            group.project_label = Some(format!("{raw_label} @ {}", source.display_label()));
        }
    }
    report
}

fn logical_identity(
    value: &str,
    source_id: &NodeId,
    logical_threads: &LogicalThreadProjection,
) -> Option<String> {
    logical_threads
        .get(&(source_id.as_str().to_owned(), value.to_owned()))
        .cloned()
}

fn scoped_value(value: &str, source_id: &NodeId) -> String {
    format!("{value}@{}", source_id.as_str())
}

pub(super) fn aggregate_source_weekly_points(
    sources: &[SourceSlice],
    account_quota: &[QuotaPoint],
    since: DateTime<Utc>,
) -> io::Result<Vec<WeeklyLocalPoint>> {
    let trace = process_trace_log().span_with("history.weekly_aggregate", || {
        TraceFields::new()
            .usize("sourceCount", sources.len())
            .usize("quotaPointCount", account_quota.len())
    });
    match aggregate_source_weekly_points_with_work(sources, account_quota, since) {
        Ok((points, work)) => {
            trace.finish_with(TraceOutcome::Ok, || {
                TraceFields::new()
                    .usize("resetCycles", work.reset_cycles)
                    .usize("timelinePoints", work.timeline_points)
                    .usize("sourceEvaluations", work.source_evaluations)
                    .usize("weeklyAdvances", work.weekly_advances)
                    .usize("bucketAdvances", work.bucket_advances)
                    .usize("coverageAdvances", work.coverage_advances)
                    .usize(
                        "weeklyPartitionEvaluations",
                        work.weekly_partition_evaluations,
                    )
                    .usize(
                        "bucketPartitionEvaluations",
                        work.bucket_partition_evaluations,
                    )
                    .usize("outputPoints", points.len())
            });
            Ok(points)
        }
        Err(error) => {
            trace.finish(TraceOutcome::Error, TraceFields::new());
            Err(error)
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct WeeklyAggregationWork {
    pub(super) reset_cycles: usize,
    pub(super) timeline_points: usize,
    pub(super) source_evaluations: usize,
    pub(super) weekly_advances: usize,
    pub(super) bucket_advances: usize,
    pub(super) coverage_advances: usize,
    pub(super) weekly_partition_evaluations: usize,
    pub(super) bucket_partition_evaluations: usize,
}

pub(super) fn aggregate_source_weekly_points_with_work(
    sources: &[SourceSlice],
    account_quota: &[QuotaPoint],
    since: DateTime<Utc>,
) -> io::Result<(Vec<WeeklyLocalPoint>, WeeklyAggregationWork)> {
    let mut work = WeeklyAggregationWork::default();
    let resets = canonical_weekly_resets(sources, account_quota)?;
    work.reset_cycles = resets.len();
    let cycle_plans = weekly_cycle_plans(sources, &resets, &mut work)?;
    let mut points = Vec::new();
    for cycle_plan in cycle_plans {
        let resets_at = cycle_plan.resets_at;
        points
            .try_reserve(cycle_plan.timeline.len())
            .map_err(|error| {
                io::Error::other(format!(
                    "could not allocate weekly projection output: {error}"
                ))
            })?;
        let mut cursors = Vec::new();
        cursors
            .try_reserve(cycle_plan.source_inputs.len())
            .map_err(|error| {
                io::Error::other(format!("could not allocate weekly source cursors: {error}"))
            })?;
        for (input, source) in cycle_plan.source_inputs.into_iter().zip(sources) {
            cursors.push(WeeklySourceCursor::from_partitioned(
                source.metadata.kind(),
                cycle_plan.starts_at,
                input,
            ));
        }

        for observed_at in cycle_plan
            .timeline
            .into_iter()
            .filter(|observed_at| *observed_at >= since && *observed_at < resets_at)
        {
            work.timeline_points = work.timeline_points.saturating_add(1);
            let mut aggregate = WeeklyAccumulator::default();
            for cursor in &mut cursors {
                work.source_evaluations = work.source_evaluations.saturating_add(1);
                if let Some(component) = cursor.advance_to(observed_at, &mut work) {
                    aggregate.add_assign(component);
                }
            }
            if !aggregate.present {
                continue;
            }
            if aggregate.estimator_revisions.len() > 1 {
                aggregate
                    .partial_reasons
                    .insert("estimator_revision_changed".to_string());
            }
            points.push(WeeklyLocalPoint {
                observed_at,
                resets_at,
                token_usage: aggregate.token_usage,
                estimated_cost_units: aggregate.estimated_cost_units,
                api_long_context_extra_cost_units: aggregate.api_long_context_extra_cost_units,
                long_context_usage_unknown: aggregate.long_context_usage_unknown,
                estimator_revision: aggregate
                    .estimator_revisions
                    .iter()
                    .next_back()
                    .copied()
                    .unwrap_or_default(),
                call_count: aggregate.call_count,
                partial_reasons: aggregate.partial_reasons.into_iter().collect(),
            });
        }
    }
    points.sort_by_key(|point| (point.observed_at, point.resets_at));
    Ok((points, work))
}

#[derive(Default)]
struct WeeklyCycleSourceInput<'a> {
    weekly: Vec<&'a WeeklyLocalPoint>,
    buckets: Vec<&'a LocalHalfHourBucket>,
    coverage_buckets: Vec<&'a LocalHalfHourBucket>,
}

struct WeeklyCyclePlan<'a> {
    starts_at: DateTime<Utc>,
    resets_at: DateTime<Utc>,
    timeline: Vec<DateTime<Utc>>,
    source_inputs: Vec<WeeklyCycleSourceInput<'a>>,
}

fn weekly_projection_limit_error(detail: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("weekly history projection exceeds its resource bound: {detail}"),
    )
}

fn reserve_weekly_membership<T>(values: &mut Vec<T>, memberships: &mut usize) -> io::Result<()> {
    *memberships = memberships
        .checked_add(1)
        .ok_or_else(|| weekly_projection_limit_error("projection membership count overflowed"))?;
    if *memberships > MAX_WEEKLY_PROJECTION_MEMBERSHIPS {
        return Err(weekly_projection_limit_error(&format!(
            "more than {MAX_WEEKLY_PROJECTION_MEMBERSHIPS} indexed memberships"
        )));
    }
    values.try_reserve(1).map_err(|error| {
        io::Error::other(format!(
            "could not allocate weekly projection index: {error}"
        ))
    })
}

/// Partitions every weekly point once and locates each bucket's applicable
/// cycle interval with binary search. This avoids rescanning every source's
/// complete record set once per canonical reset.
fn weekly_cycle_plans<'a>(
    sources: &'a [SourceSlice],
    resets: &[DateTime<Utc>],
    work: &mut WeeklyAggregationWork,
) -> io::Result<Vec<WeeklyCyclePlan<'a>>> {
    let mut plans = Vec::new();
    plans.try_reserve(resets.len()).map_err(|error| {
        io::Error::other(format!(
            "could not allocate weekly cycle projection: {error}"
        ))
    })?;
    for resets_at in resets.iter().copied() {
        let mut source_inputs = Vec::new();
        source_inputs.try_reserve(sources.len()).map_err(|error| {
            io::Error::other(format!(
                "could not allocate weekly source projection: {error}"
            ))
        })?;
        source_inputs.resize_with(sources.len(), WeeklyCycleSourceInput::default);
        plans.push(WeeklyCyclePlan {
            starts_at: resets_at
                .checked_sub_signed(Duration::minutes(WEEKLY_WINDOW_MINUTES))
                .unwrap_or(DateTime::<Utc>::MIN_UTC),
            resets_at,
            timeline: Vec::new(),
            source_inputs,
        });
    }

    let weekly_window = Duration::minutes(WEEKLY_WINDOW_MINUTES);
    let mut memberships = 0_usize;
    for (source_index, source) in sources.iter().enumerate() {
        for point in &source.weekly_local_points {
            work.weekly_partition_evaluations = work.weekly_partition_evaluations.saturating_add(1);
            let Some(resets_at) = assigned_canonical_reset(point.resets_at, resets) else {
                continue;
            };
            let Ok(cycle_index) = resets.binary_search(&resets_at) else {
                return Err(weekly_projection_limit_error(
                    "assigned reset is missing from the canonical index",
                ));
            };
            let plan = &mut plans[cycle_index];
            if point.observed_at < plan.starts_at || point.observed_at >= plan.resets_at {
                continue;
            }
            let input = &mut plan.source_inputs[source_index];
            reserve_weekly_membership(&mut input.weekly, &mut memberships)?;
            input.weekly.push(point);
            reserve_weekly_membership(&mut plan.timeline, &mut memberships)?;
            plan.timeline.push(point.observed_at);
        }

        for bucket in &source.buckets {
            work.bucket_partition_evaluations = work.bucket_partition_evaluations.saturating_add(1);
            let complete_start = resets.partition_point(|reset| *reset < bucket.ends_at);
            let complete_end = bucket
                .starts_at
                .checked_add_signed(weekly_window)
                .map_or(resets.len(), |latest_reset| {
                    resets.partition_point(|reset| *reset <= latest_reset)
                });
            for plan in plans.iter_mut().take(complete_end).skip(complete_start) {
                let input = &mut plan.source_inputs[source_index];
                reserve_weekly_membership(&mut input.buckets, &mut memberships)?;
                input.buckets.push(bucket);
                reserve_weekly_membership(&mut plan.timeline, &mut memberships)?;
                plan.timeline.push(bucket.ends_at);
            }

            let coverage_start = resets.partition_point(|reset| *reset <= bucket.starts_at);
            let coverage_end = bucket
                .ends_at
                .checked_add_signed(weekly_window)
                .map_or(resets.len(), |latest_reset| {
                    resets.partition_point(|reset| *reset < latest_reset)
                });
            for plan in plans.iter_mut().take(coverage_end).skip(coverage_start) {
                let coverage = &mut plan.source_inputs[source_index].coverage_buckets;
                reserve_weekly_membership(coverage, &mut memberships)?;
                coverage.push(bucket);
            }
        }
    }

    for plan in &mut plans {
        plan.timeline.sort_unstable();
        plan.timeline.dedup();
        for input in &mut plan.source_inputs {
            input.weekly.sort_unstable_by_key(|point| point.observed_at);
            input
                .buckets
                .sort_unstable_by_key(|bucket| (bucket.ends_at, bucket.starts_at));
            input
                .coverage_buckets
                .sort_unstable_by_key(|bucket| (bucket.starts_at, bucket.ends_at));
        }
    }
    Ok(plans)
}

pub(super) fn canonical_weekly_resets(
    sources: &[SourceSlice],
    account_quota: &[QuotaPoint],
) -> io::Result<Vec<DateTime<Utc>>> {
    // A server reports `observed_at + 7d` while a completely unused weekly
    // window is not anchored yet. Persisted samples of that rolling estimate
    // must not each become a distinct cycle. Non-zero account observations
    // are authoritative; retain only the newest zero-evidence candidate so an
    // actually idle current cycle can still be represented.
    let latest_anchored_observation = account_quota
        .iter()
        .filter(|point| {
            point.duration_mins == WEEKLY_WINDOW_MINUTES
                && point.limit_id.trim().eq_ignore_ascii_case("codex")
                && quota_point_establishes_weekly_cycle(point)
        })
        .map(|point| point.observed_at)
        .chain(
            sources
                .iter()
                .flat_map(|source| &source.weekly_local_points)
                .filter(|point| weekly_point_establishes_cycle(point))
                .map(|point| point.observed_at),
        )
        .max();
    let mut account_candidates = Vec::new();
    account_candidates
        .try_reserve(account_quota.len())
        .map_err(|error| {
            io::Error::other(format!(
                "could not allocate account weekly reset candidates: {error}"
            ))
        })?;
    account_candidates.extend(account_quota.iter().filter_map(|point| {
        (point.duration_mins == WEEKLY_WINDOW_MINUTES
            && point.limit_id.trim().eq_ignore_ascii_case("codex")
            && quota_point_establishes_weekly_cycle(point))
        .then_some(point.resets_at)
    }));
    account_candidates.sort_unstable();
    let account_resets = cluster_sorted_weekly_resets(&account_candidates, 0)?;

    let source_candidate_count = sources.iter().try_fold(0_usize, |total, source| {
        total
            .checked_add(source.weekly_local_points.len())
            .ok_or_else(|| weekly_projection_limit_error("reset candidate count overflowed"))
    })?;
    let mut source_candidates = Vec::new();
    source_candidates
        .try_reserve(source_candidate_count)
        .map_err(|error| {
            io::Error::other(format!(
                "could not allocate source weekly reset candidates: {error}"
            ))
        })?;
    source_candidates.extend(
        sources
            .iter()
            .flat_map(|source| &source.weekly_local_points)
            .filter(|point| weekly_point_establishes_cycle(point))
            .map(|point| point.resets_at)
            // An account reset is authoritative even when its timestamp is
            // later than the source candidate, so filter against the complete
            // account set before clustering source-only timestamps.
            .filter(|candidate| assigned_canonical_reset(*candidate, &account_resets).is_none()),
    );
    source_candidates.sort_unstable();
    let source_resets = cluster_sorted_weekly_resets(&source_candidates, account_resets.len())?;

    let mut resets = account_resets;
    resets.try_reserve(source_resets.len()).map_err(|error| {
        io::Error::other(format!(
            "could not allocate canonical weekly reset index: {error}"
        ))
    })?;
    resets.extend(source_resets);
    resets.sort_unstable();
    if resets.len() > MAX_WEEKLY_RESET_CYCLES {
        return Err(weekly_projection_limit_error(&format!(
            "more than {MAX_WEEKLY_RESET_CYCLES} canonical reset cycles in retained history"
        )));
    }

    let latest_unanchored = account_quota
        .iter()
        .filter(|point| {
            point.duration_mins == WEEKLY_WINDOW_MINUTES
                && point.limit_id.trim().eq_ignore_ascii_case("codex")
                && !quota_point_establishes_weekly_cycle(point)
        })
        .map(|point| (point.observed_at, true, point.resets_at))
        .chain(
            sources
                .iter()
                .flat_map(|source| &source.weekly_local_points)
                .filter(|point| !weekly_point_establishes_cycle(point))
                .map(|point| (point.observed_at, false, point.resets_at)),
        )
        // Prefer the account timestamp on an exact observation tie.
        .max_by_key(|(observed_at, account, _)| (*observed_at, *account));
    if let Some((observed_at, _, candidate)) = latest_unanchored
        && latest_anchored_observation.is_none_or(|anchored| observed_at > anchored)
        && assigned_canonical_reset(candidate, &resets).is_none()
    {
        if resets.len() >= MAX_WEEKLY_RESET_CYCLES {
            return Err(weekly_projection_limit_error(&format!(
                "more than {MAX_WEEKLY_RESET_CYCLES} canonical reset cycles in retained history"
            )));
        }
        resets.try_reserve(1).map_err(|error| {
            io::Error::other(format!(
                "could not allocate canonical weekly reset index: {error}"
            ))
        })?;
        let insertion = resets.partition_point(|reset| *reset < candidate);
        resets.insert(insertion, candidate);
    }
    Ok(resets)
}

fn cluster_sorted_weekly_resets(
    candidates: &[DateTime<Utc>],
    existing_cycle_count: usize,
) -> io::Result<Vec<DateTime<Utc>>> {
    let available = MAX_WEEKLY_RESET_CYCLES
        .checked_sub(existing_cycle_count)
        .ok_or_else(|| {
            weekly_projection_limit_error(&format!(
                "more than {MAX_WEEKLY_RESET_CYCLES} canonical reset cycles in retained history"
            ))
        })?;
    let mut resets = Vec::new();
    resets
        .try_reserve(available.min(candidates.len()))
        .map_err(|error| {
            io::Error::other(format!(
                "could not allocate canonical weekly reset index: {error}"
            ))
        })?;
    for candidate in candidates.iter().copied() {
        if resets
            .last()
            .is_some_and(|existing| reset_matches(*existing, candidate))
        {
            continue;
        }
        if resets.len() >= available {
            return Err(weekly_projection_limit_error(&format!(
                "more than {MAX_WEEKLY_RESET_CYCLES} canonical reset cycles in retained history"
            )));
        }
        resets.push(candidate);
    }
    Ok(resets)
}

fn quota_point_establishes_weekly_cycle(point: &QuotaPoint) -> bool {
    point.used_percent > 0.0 || point.remaining_percent < 100.0
}

fn weekly_point_establishes_cycle(point: &WeeklyLocalPoint) -> bool {
    !point.token_usage.is_zero()
        || point.estimated_cost_units > 0
        || point.call_count > 0
        || point
            .partial_reasons
            .iter()
            .any(|reason| reason == DUPLICATE_SESSION_WEEKLY_REBUILT_FROM_BUCKETS)
}

fn reset_matches(left: DateTime<Utc>, right: DateTime<Utc>) -> bool {
    left.signed_duration_since(right)
        .num_seconds()
        .unsigned_abs()
        <= RESET_DRIFT_SECONDS as u64
}

/// Assigns a reported reset to at most one canonical cycle. Reset timestamps
/// from different machines may straddle two tolerance windows; independently
/// matching every cycle would count the same cumulative weekly baseline twice.
/// The nearest anchor wins, with the earlier anchor breaking an exact tie.
pub(super) fn assigned_canonical_reset(
    reported: DateTime<Utc>,
    canonical: &[DateTime<Utc>],
) -> Option<DateTime<Utc>> {
    // `canonical_weekly_resets` returns a sorted set. Only the neighbours at
    // the insertion point can be nearest; avoiding a full scan matters when a
    // large source history is projected across several retained cycles.
    let insertion = canonical.partition_point(|candidate| *candidate < reported);
    let earlier = insertion
        .checked_sub(1)
        .and_then(|index| canonical.get(index))
        .copied();
    let later = canonical.get(insertion).copied();
    [earlier, later]
        .into_iter()
        .flatten()
        .filter(|candidate| reset_matches(reported, *candidate))
        .min_by_key(|candidate| {
            (
                reported
                    .signed_duration_since(*candidate)
                    .num_seconds()
                    .unsigned_abs(),
                *candidate,
            )
        })
}

pub(super) struct WeeklySourceCursor<'a> {
    source_kind: SourceKind,
    cycle_starts_at: DateTime<Utc>,
    weekly: Vec<&'a WeeklyLocalPoint>,
    buckets: Vec<&'a LocalHalfHourBucket>,
    coverage_buckets: Vec<&'a LocalHalfHourBucket>,
    weekly_index: usize,
    bucket_index: usize,
    coverage_index: usize,
    base_observed_at: Option<DateTime<Utc>>,
    aggregate: WeeklyAccumulator,
    coverage_through: DateTime<Utc>,
    coverage_gap: bool,
}

impl<'a> WeeklySourceCursor<'a> {
    #[cfg(test)]
    pub(super) fn new(
        source: &'a SourceSlice,
        cycle_starts_at: DateTime<Utc>,
        resets_at: DateTime<Utc>,
    ) -> Self {
        Self::new_assigned(source, cycle_starts_at, resets_at, &[resets_at])
    }

    #[cfg(test)]
    fn new_assigned(
        source: &'a SourceSlice,
        cycle_starts_at: DateTime<Utc>,
        resets_at: DateTime<Utc>,
        canonical_resets: &[DateTime<Utc>],
    ) -> Self {
        let mut weekly = source
            .weekly_local_points
            .iter()
            .filter(|point| {
                assigned_canonical_reset(point.resets_at, canonical_resets) == Some(resets_at)
            })
            .filter(|point| point.observed_at >= cycle_starts_at)
            .filter(|point| point.observed_at < resets_at)
            .collect::<Vec<_>>();
        weekly.sort_by_key(|point| point.observed_at);
        let mut buckets = source
            .buckets
            .iter()
            .filter(|bucket| bucket.starts_at >= cycle_starts_at && bucket.ends_at <= resets_at)
            .collect::<Vec<_>>();
        buckets.sort_by_key(|bucket| (bucket.ends_at, bucket.starts_at));
        let mut coverage_buckets = source
            .buckets
            .iter()
            .filter(|bucket| bucket.ends_at > cycle_starts_at && bucket.starts_at < resets_at)
            .collect::<Vec<_>>();
        coverage_buckets.sort_by_key(|bucket| (bucket.starts_at, bucket.ends_at));
        Self::from_partitioned(
            source.metadata.kind(),
            cycle_starts_at,
            WeeklyCycleSourceInput {
                weekly,
                buckets,
                coverage_buckets,
            },
        )
    }

    fn from_partitioned(
        source_kind: SourceKind,
        cycle_starts_at: DateTime<Utc>,
        input: WeeklyCycleSourceInput<'a>,
    ) -> Self {
        Self {
            source_kind,
            cycle_starts_at,
            weekly: input.weekly,
            buckets: input.buckets,
            coverage_buckets: input.coverage_buckets,
            weekly_index: 0,
            bucket_index: 0,
            coverage_index: 0,
            base_observed_at: None,
            aggregate: WeeklyAccumulator::default(),
            coverage_through: cycle_starts_at,
            coverage_gap: false,
        }
    }

    pub(super) fn advance_to(
        &mut self,
        observed_at: DateTime<Utc>,
        work: &mut WeeklyAggregationWork,
    ) -> Option<WeeklyAccumulator> {
        while self
            .weekly
            .get(self.weekly_index)
            .is_some_and(|point| point.observed_at <= observed_at)
        {
            let point = self.weekly[self.weekly_index];
            self.aggregate = WeeklyAccumulator::default();
            self.aggregate.add_weekly(point);
            if point.observed_at.timestamp().rem_euclid(15 * 60) != 0 {
                self.aggregate
                    .partial_reasons
                    .insert("weekly_source_boundary_excludes_partial_bucket".to_string());
            }
            self.base_observed_at = Some(point.observed_at);
            self.weekly_index += 1;
            work.weekly_advances = work.weekly_advances.saturating_add(1);
        }

        let bucket_cutoff = self.base_observed_at.unwrap_or(self.cycle_starts_at);
        while self
            .buckets
            .get(self.bucket_index)
            .is_some_and(|bucket| bucket.ends_at <= observed_at)
        {
            let bucket = self.buckets[self.bucket_index];
            if bucket.starts_at >= bucket_cutoff {
                self.aggregate.add_bucket(bucket);
            }
            self.bucket_index += 1;
            work.bucket_advances = work.bucket_advances.saturating_add(1);
        }

        if !self.aggregate.present {
            return None;
        }
        let mut aggregate = self.aggregate.clone();
        if self.base_observed_at.is_none() {
            if self.source_kind == SourceKind::Ssh {
                aggregate
                    .partial_reasons
                    .insert("remote_weekly_from_buckets_lower_bound".to_string());
            } else if !self.coverage_complete_through(observed_at, work) {
                aggregate
                    .partial_reasons
                    .insert("local_weekly_from_buckets_lower_bound".to_string());
            }
        }
        Some(aggregate)
    }

    fn coverage_complete_through(
        &mut self,
        observed_at: DateTime<Utc>,
        work: &mut WeeklyAggregationWork,
    ) -> bool {
        let target = observed_at - Duration::seconds(observed_at.timestamp().rem_euclid(15 * 60));
        if target <= self.cycle_starts_at {
            return target == self.cycle_starts_at;
        }
        while self
            .coverage_buckets
            .get(self.coverage_index)
            .is_some_and(|bucket| bucket.starts_at < target)
        {
            let bucket = self.coverage_buckets[self.coverage_index];
            if !self.coverage_gap {
                if bucket.starts_at > self.coverage_through {
                    self.coverage_gap = true;
                } else if bucket.ends_at > self.coverage_through {
                    self.coverage_through = bucket.ends_at;
                }
            }
            self.coverage_index += 1;
            work.coverage_advances = work.coverage_advances.saturating_add(1);
        }
        !self.coverage_gap && self.coverage_through >= target
    }
}

#[cfg(test)]
pub(super) fn source_weekly_cumulative_at(
    source: &SourceSlice,
    cycle_starts_at: DateTime<Utc>,
    resets_at: DateTime<Utc>,
    observed_at: DateTime<Utc>,
) -> Option<WeeklyAccumulator> {
    let base = source
        .weekly_local_points
        .iter()
        .filter(|point| reset_matches(point.resets_at, resets_at))
        .filter(|point| point.observed_at <= observed_at)
        .max_by_key(|point| point.observed_at);
    let bucket_cutoff = base.map_or(cycle_starts_at, |point| point.observed_at);
    let mut aggregate = WeeklyAccumulator::default();
    if let Some(point) = base {
        aggregate.add_weekly(point);
        if point.observed_at.timestamp().rem_euclid(15 * 60) != 0 {
            aggregate
                .partial_reasons
                .insert("weekly_source_boundary_excludes_partial_bucket".to_string());
        }
    }
    for bucket in source.buckets.iter().filter(|bucket| {
        bucket.starts_at >= cycle_starts_at
            && bucket.starts_at >= bucket_cutoff
            && bucket.ends_at <= observed_at
            && bucket.ends_at <= resets_at
    }) {
        aggregate.add_bucket(bucket);
    }
    if aggregate.present && base.is_none() {
        if source.metadata.kind() == SourceKind::Ssh {
            aggregate
                .partial_reasons
                .insert("remote_weekly_from_buckets_lower_bound".to_string());
        } else if !source_buckets_cover_cycle(
            &source.buckets,
            cycle_starts_at,
            observed_at.min(resets_at),
        ) {
            aggregate
                .partial_reasons
                .insert("local_weekly_from_buckets_lower_bound".to_string());
        }
    }
    aggregate.present.then_some(aggregate)
}

#[cfg(test)]
fn source_buckets_cover_cycle(
    buckets: &[LocalHalfHourBucket],
    cycle_starts_at: DateTime<Utc>,
    observed_through: DateTime<Utc>,
) -> bool {
    let target =
        observed_through - Duration::seconds(observed_through.timestamp().rem_euclid(15 * 60));
    if target <= cycle_starts_at {
        return target == cycle_starts_at;
    }
    let mut covered_through = cycle_starts_at;
    for bucket in buckets
        .iter()
        .filter(|bucket| bucket.ends_at > cycle_starts_at && bucket.starts_at < target)
    {
        if bucket.starts_at > covered_through {
            return false;
        }
        if bucket.ends_at > covered_through {
            covered_through = bucket.ends_at;
        }
        if covered_through >= target {
            return true;
        }
    }
    false
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct WeeklyAccumulator {
    pub(super) present: bool,
    pub(super) token_usage: TokenUsage,
    pub(super) estimated_cost_units: u128,
    pub(super) api_long_context_extra_cost_units: Option<u128>,
    pub(super) long_context_usage_unknown: bool,
    pub(super) estimator_revisions: BTreeSet<u32>,
    pub(super) call_count: u64,
    pub(super) partial_reasons: BTreeSet<String>,
}

impl WeeklyAccumulator {
    fn add_weekly(&mut self, point: &WeeklyLocalPoint) {
        self.add_values(
            point.token_usage,
            point.estimated_cost_units,
            point.api_long_context_extra_cost_units,
            point.long_context_usage_unknown,
            point.estimator_revision,
            point.call_count,
            &point.partial_reasons,
        );
    }

    fn add_bucket(&mut self, bucket: &LocalHalfHourBucket) {
        self.add_values(
            bucket.token_usage,
            bucket.estimated_cost_units,
            bucket.api_long_context_extra_cost_units,
            bucket.long_context_usage_unknown,
            bucket.estimator_revision,
            bucket.call_count,
            &bucket.partial_reasons,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn add_values(
        &mut self,
        token_usage: TokenUsage,
        estimated_cost_units: u128,
        api_long_context_extra_cost_units: Option<u128>,
        long_context_usage_unknown: bool,
        estimator_revision: u32,
        call_count: u64,
        partial_reasons: &[String],
    ) {
        if !self.present {
            self.api_long_context_extra_cost_units = Some(0);
        }
        self.present = true;
        self.token_usage.add_assign(token_usage);
        self.estimated_cost_units = self
            .estimated_cost_units
            .saturating_add(estimated_cost_units);
        self.api_long_context_extra_cost_units = add_optional_units(
            self.api_long_context_extra_cost_units,
            api_long_context_extra_cost_units,
        );
        self.long_context_usage_unknown |= long_context_usage_unknown;
        self.estimator_revisions.insert(estimator_revision);
        self.call_count = self.call_count.saturating_add(call_count);
        self.partial_reasons.extend(partial_reasons.iter().cloned());
    }

    fn add_assign(&mut self, other: Self) {
        if !other.present {
            return;
        }
        if !self.present {
            self.api_long_context_extra_cost_units = Some(0);
        }
        self.present = true;
        self.token_usage.add_assign(other.token_usage);
        self.estimated_cost_units = self
            .estimated_cost_units
            .saturating_add(other.estimated_cost_units);
        self.api_long_context_extra_cost_units = add_optional_units(
            self.api_long_context_extra_cost_units,
            other.api_long_context_extra_cost_units,
        );
        self.long_context_usage_unknown |= other.long_context_usage_unknown;
        self.estimator_revisions.extend(other.estimator_revisions);
        self.call_count = self.call_count.saturating_add(other.call_count);
        self.partial_reasons.extend(other.partial_reasons);
    }
}
