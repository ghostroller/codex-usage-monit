//! Logical-replica-safe remote history projection for the TUI Overview.
//!
//! The input is the independent `AllIncluded` unified history query, never a
//! direct sum of per-source SSH buckets. `history_query` has already resolved
//! logical replicas and selected one authority before this projection runs.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};

use crate::api_cost::API_PRICING_CATALOG_REVISION;
use crate::domain::{
    ApiCostAmount, Confidence, Provenance, TaskRecord, TaskStatus, ThreadWindowUsage, TokenUsage,
    TurnRecord, TurnStatus, TurnWindowUsage, WindowAnalysis, WindowUsage,
};
use crate::history::{
    HISTORY_PROJECT_BREAKDOWN_REVISION, HistoryData, LocalHalfHourBucket, LocalProjectUsageGroup,
};
use crate::source_identity::NodeId;

const LOGICAL_THREAD_PREFIX: &str = "logical-thread:";

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct RemoteOverviewHistory {
    buckets: Arc<Vec<LocalHalfHourBucket>>,
    warnings: Arc<Vec<String>>,
    /// Included SSH node IDs and presentation labels. IDs classify scoped
    /// `raw@node` identities; labels are used only for visible source names.
    remote_sources: BTreeMap<String, String>,
}

impl RemoteOverviewHistory {
    pub(crate) fn from_unified(
        history: &HistoryData,
        remote_sources: impl IntoIterator<Item = (NodeId, String)>,
        as_of: DateTime<Utc>,
    ) -> Self {
        let remote_sources = remote_sources
            .into_iter()
            .map(|(node, label)| (node.as_str().to_owned(), label))
            .collect::<BTreeMap<_, _>>();
        let since = as_of
            .checked_sub_signed(Duration::days(8))
            .unwrap_or(DateTime::<Utc>::MIN_UTC);
        let missing_project_breakdown = history.half_hour_buckets.iter().any(|bucket| {
            bucket.ends_at > since
                && bucket.sampled_at <= as_of
                && bucket.project_breakdown_revision != HISTORY_PROJECT_BREAKDOWN_REVISION
                && bucket.project_groups.iter().any(|group| {
                    classify_thread(&group.thread_id, &remote_sources).is_some()
                        && !group.token_usage.is_zero()
                })
        });
        let buckets = history
            .half_hour_buckets
            .iter()
            .filter(|bucket| bucket.ends_at > since && bucket.sampled_at <= as_of)
            .filter_map(|bucket| {
                let mut bucket = bucket.clone();
                bucket.groups.clear();
                bucket
                    .project_groups
                    .retain(|group| classify_thread(&group.thread_id, &remote_sources).is_some());
                (!bucket.project_groups.is_empty()).then_some(bucket)
            })
            .collect();
        let mut warnings = history
            .warnings
            .iter()
            .map(|warning| format!("remote history unified query: {warning}"))
            .collect::<Vec<_>>();
        if missing_project_breakdown {
            warnings.push(
                "remote history unified query: project breakdown unavailable for retained usage"
                    .to_owned(),
            );
        }
        warnings.sort();
        warnings.dedup();
        Self {
            buckets: Arc::new(buckets),
            warnings: Arc::new(warnings),
            remote_sources,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct RemoteOverviewProjection {
    pub(crate) tasks: Vec<TaskRecord>,
    pub(crate) turns: Vec<TurnRecord>,
    pub(crate) windows: Vec<RemoteOverviewWindow>,
    pub(crate) warnings: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct RemoteOverviewWindow {
    pub(crate) duration_mins: i64,
    pub(crate) threads: Vec<ThreadWindowUsage>,
    pub(crate) turns: Vec<TurnWindowUsage>,
    /// Canonical logical threads replace the collector's raw local row rather
    /// than being added to it. This is the key to replica-safe overlay.
    pub(crate) replaced_local_threads: BTreeSet<String>,
    pub(crate) partial_reasons: Vec<String>,
}

#[derive(Clone, Debug, Default)]
struct UsageAggregate {
    tokens: TokenUsage,
    api_cost: ApiCostAmount,
}

impl UsageAggregate {
    fn add_group(&mut self, group: &LocalProjectUsageGroup, pricing_catalog_current: bool) {
        self.tokens.add_assign(group.token_usage);
        self.api_cost.add_assign(if pricing_catalog_current {
            group.api_equivalent_cost
        } else {
            unpriced_api_cost(group)
        });
    }

    fn window_usage(&self) -> WindowUsage {
        WindowUsage {
            token_usage: self.tokens,
            local_token_share_percent: 0.0,
            // The persisted Snapshot does not expose the collector's absolute
            // local cost-unit denominator. Keep remote quota share unknown
            // rather than mixing incompatible denominators.
            estimated_quota_percent: 0.0,
            quota_confidence: Confidence::Unknown,
            api_equivalent_cost: self.api_cost,
        }
    }
}

fn unpriced_api_cost(group: &LocalProjectUsageGroup) -> ApiCostAmount {
    ApiCostAmount {
        observed_samples: group
            .api_equivalent_cost
            .observed_samples
            .max(group.call_count),
        observed_tokens: group
            .api_equivalent_cost
            .observed_tokens
            .max(group.token_usage.total_tokens),
        ..ApiCostAmount::default()
    }
}

#[derive(Clone, Debug, Default)]
struct TaskMetadata {
    thread_id: String,
    parent_thread_id: Option<String>,
    parent_thread_conflict: bool,
    session_thread_id: Option<String>,
    session_thread_conflict: bool,
    source: Option<String>,
    title: Option<String>,
    project_label: Option<String>,
    created_at: Option<DateTime<Utc>>,
    updated_at: Option<DateTime<Utc>>,
    turns: BTreeSet<String>,
    usage: UsageAggregate,
}

#[derive(Clone, Debug, Default)]
struct TurnMetadata {
    thread_id: String,
    turn_id: String,
    message_preview: Option<String>,
    started_at: Option<DateTime<Utc>>,
    usage: UsageAggregate,
}

#[derive(Clone, Debug)]
struct ClassifiedThread {
    canonical: String,
    source: Option<String>,
    logical: bool,
}

pub(crate) fn project_remote_overview_history(
    history: &RemoteOverviewHistory,
    analyses: &[WindowAnalysis],
    as_of: DateTime<Utc>,
) -> RemoteOverviewProjection {
    let windows = analyses
        .iter()
        .filter_map(|analysis| {
            analysis
                .attribution
                .window
                .as_ref()
                .map(|window| (analysis.duration_mins, window.starts_at, window.ends_at))
        })
        .collect::<Vec<_>>();
    let mut tasks = BTreeMap::<String, TaskMetadata>::new();
    let mut turns = BTreeMap::<(String, String), TurnMetadata>::new();
    let mut window_threads = windows
        .iter()
        .map(|(duration, _, _)| (*duration, BTreeMap::<String, UsageAggregate>::new()))
        .collect::<BTreeMap<_, _>>();
    let mut window_turns = windows
        .iter()
        .map(|(duration, _, _)| {
            (
                *duration,
                BTreeMap::<(String, String), UsageAggregate>::new(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let mut replaced_local_threads = BTreeMap::<i64, BTreeSet<String>>::new();
    let mut window_reasons = BTreeMap::<i64, BTreeSet<String>>::new();
    for (duration, starts_at, ends_at) in &windows {
        if history.buckets.iter().any(|bucket| {
            bucket.sampled_at <= as_of
                && ((bucket.starts_at < *starts_at && bucket.ends_at > *starts_at)
                    || (bucket.starts_at < *ends_at && bucket.ends_at > *ends_at))
        }) {
            window_reasons
                .entry(*duration)
                .or_default()
                .insert("remote_window_boundary_lower_bound".to_owned());
        }
        if !history.warnings.is_empty() {
            window_reasons
                .entry(*duration)
                .or_default()
                .insert("remote_unified_history_warning".to_owned());
        }
    }

    for bucket in history.buckets.iter() {
        let selected_windows = windows
            .iter()
            .filter(|(_, starts_at, ends_at)| {
                bucket.starts_at >= *starts_at
                    && bucket.starts_at < *ends_at
                    && (bucket.ends_at <= *ends_at || bucket.sampled_at <= *ends_at)
                    && bucket.sampled_at <= as_of
            })
            .map(|(duration, _, _)| *duration)
            .collect::<Vec<_>>();
        if selected_windows.is_empty() {
            continue;
        }
        for group in &bucket.project_groups {
            let Some(classified) = classify_thread(&group.thread_id, &history.remote_sources)
            else {
                continue;
            };
            let parent = group.parent_thread_id.as_deref().and_then(|value| {
                canonical_reference(value, &history.remote_sources, classified.source.as_deref())
            });
            let session = group.session_thread_id.as_deref().and_then(|value| {
                canonical_reference(value, &history.remote_sources, classified.source.as_deref())
            });
            let turn_id = group
                .turn_id
                .as_deref()
                .map(strip_scoped_value)
                .map(str::to_owned);
            let delegated = parent.is_some();
            {
                let task = tasks
                    .entry(classified.canonical.clone())
                    .or_insert_with(|| TaskMetadata {
                        thread_id: classified.canonical.clone(),
                        source: classified.source.clone(),
                        ..TaskMetadata::default()
                    });
                merge_task_metadata(task, group, bucket, parent.clone(), session.clone());
                task.usage.add_group(
                    group,
                    bucket.api_pricing_catalog_revision == API_PRICING_CATALOG_REVISION,
                );
                if let Some(turn_id) = turn_id.as_ref() {
                    task.turns.insert(turn_id.clone());
                }
            }
            ensure_placeholder(
                &mut tasks,
                parent.as_deref(),
                session.as_deref(),
                classified.source.as_deref(),
            );
            ensure_placeholder(
                &mut tasks,
                session.as_deref(),
                None,
                classified.source.as_deref(),
            );

            if let Some(turn_id) = turn_id.as_ref() {
                let turn = turns
                    .entry((classified.canonical.clone(), turn_id.clone()))
                    .or_insert_with(|| TurnMetadata {
                        thread_id: classified.canonical.clone(),
                        turn_id: turn_id.clone(),
                        ..TurnMetadata::default()
                    });
                merge_turn_metadata(turn, group, delegated);
                turn.usage.add_group(
                    group,
                    bucket.api_pricing_catalog_revision == API_PRICING_CATALOG_REVISION,
                );
            }

            for duration in &selected_windows {
                window_threads
                    .entry(*duration)
                    .or_default()
                    .entry(classified.canonical.clone())
                    .or_default()
                    .add_group(
                        group,
                        bucket.api_pricing_catalog_revision == API_PRICING_CATALOG_REVISION,
                    );
                if classified.logical {
                    replaced_local_threads
                        .entry(*duration)
                        .or_default()
                        .insert(classified.canonical.clone());
                }
                if let Some(turn_id) = turn_id.as_ref() {
                    window_turns
                        .entry(*duration)
                        .or_default()
                        .entry((classified.canonical.clone(), turn_id.clone()))
                        .or_default()
                        .add_group(
                            group,
                            bucket.api_pricing_catalog_revision == API_PRICING_CATALOG_REVISION,
                        );
                }
            }
        }
        for duration in selected_windows {
            if bucket.project_breakdown_revision != HISTORY_PROJECT_BREAKDOWN_REVISION {
                window_reasons
                    .entry(duration)
                    .or_default()
                    .insert("remote_project_breakdown_revision_mismatch".to_string());
            }
            if bucket.api_pricing_catalog_revision != API_PRICING_CATALOG_REVISION
                && bucket.project_groups.iter().any(|group| {
                    !group.token_usage.is_zero()
                        || group.api_equivalent_cost.observed_samples > 0
                        || group.api_equivalent_cost.observed_tokens > 0
                })
            {
                window_reasons
                    .entry(duration)
                    .or_default()
                    .insert("remote_api_pricing_catalog_revision_mismatch".to_string());
            }
            if !bucket.partial_reasons.is_empty() {
                window_reasons
                    .entry(duration)
                    .or_default()
                    .insert("remote_history_bucket_partial".to_string());
            }
        }
    }

    if tasks
        .values()
        .any(|task| task.parent_thread_conflict || task.session_thread_conflict)
    {
        for (duration, _, _) in &windows {
            window_reasons
                .entry(*duration)
                .or_default()
                .insert("remote_history_lineage_conflict".to_owned());
        }
    }
    close_ancestors(&mut tasks);
    let mut projection = RemoteOverviewProjection {
        tasks: tasks.into_values().map(task_record).collect(),
        turns: turns.into_values().map(turn_record).collect(),
        windows: Vec::new(),
        warnings: history.warnings.as_ref().clone(),
    };
    for (duration_mins, threads) in window_threads {
        projection.windows.push(RemoteOverviewWindow {
            duration_mins,
            threads: threads
                .into_iter()
                .map(|(thread_id, usage)| ThreadWindowUsage {
                    thread_id,
                    usage: usage.window_usage(),
                })
                .collect(),
            turns: window_turns
                .remove(&duration_mins)
                .unwrap_or_default()
                .into_iter()
                .map(|((thread_id, turn_id), usage)| TurnWindowUsage {
                    thread_id,
                    turn_id,
                    usage: usage.window_usage(),
                })
                .collect(),
            replaced_local_threads: replaced_local_threads
                .remove(&duration_mins)
                .unwrap_or_default(),
            partial_reasons: window_reasons
                .remove(&duration_mins)
                .unwrap_or_default()
                .into_iter()
                .collect(),
        });
    }
    projection
        .tasks
        .sort_by(|left, right| left.thread_id.cmp(&right.thread_id));
    projection.turns.sort_by(|left, right| {
        left.thread_id
            .cmp(&right.thread_id)
            .then_with(|| left.turn_id.cmp(&right.turn_id))
    });
    projection
        .windows
        .sort_by_key(|window| window.duration_mins);
    projection
}

fn classify_thread(
    value: &str,
    remote_sources: &BTreeMap<String, String>,
) -> Option<ClassifiedThread> {
    if let Some(raw) = value.strip_prefix(LOGICAL_THREAD_PREFIX) {
        return Some(ClassifiedThread {
            canonical: raw.to_owned(),
            source: Some("remote:replica".to_owned()),
            logical: true,
        });
    }
    let (raw, node) = split_scoped_value(value)?;
    let label = remote_sources.get(node)?;
    Some(ClassifiedThread {
        canonical: format!("remote:{node}:{raw}"),
        source: Some(format!("remote:{label}")),
        logical: false,
    })
}

fn canonical_reference(
    value: &str,
    remote_sources: &BTreeMap<String, String>,
    fallback_source: Option<&str>,
) -> Option<String> {
    if let Some(raw) = value.strip_prefix(LOGICAL_THREAD_PREFIX) {
        return Some(raw.to_owned());
    }
    if let Some((raw, node)) = split_scoped_value(value) {
        if remote_sources.contains_key(node) {
            return Some(format!("remote:{node}:{raw}"));
        }
        return Some(raw.to_owned());
    }
    fallback_source
        .and_then(|source| source.strip_prefix("remote:"))
        .map_or_else(|| Some(value.to_owned()), |_| Some(value.to_owned()))
}

fn split_scoped_value(value: &str) -> Option<(&str, &str)> {
    let (raw, node) = value.rsplit_once('@')?;
    node.starts_with("node-").then_some((raw, node))
}

fn strip_scoped_value(value: &str) -> &str {
    split_scoped_value(value).map_or(value, |(raw, _)| raw)
}

fn merge_task_metadata(
    task: &mut TaskMetadata,
    group: &LocalProjectUsageGroup,
    bucket: &LocalHalfHourBucket,
    parent: Option<String>,
    session: Option<String>,
) {
    merge_lineage_reference(
        &mut task.parent_thread_id,
        &mut task.parent_thread_conflict,
        parent,
    );
    merge_lineage_reference(
        &mut task.session_thread_id,
        &mut task.session_thread_conflict,
        session,
    );
    if task.title.is_none() {
        task.title = group.title.clone();
    }
    if task.project_label.is_none() {
        task.project_label = group.project_label.clone();
    }
    task.created_at = min_timestamp(task.created_at, group.turn_started_at);
    task.updated_at = max_timestamp(task.updated_at, Some(bucket.sampled_at));
}

fn merge_lineage_reference(
    current: &mut Option<String>,
    conflicted: &mut bool,
    incoming: Option<String>,
) {
    if *conflicted {
        return;
    }
    let Some(incoming) = incoming else { return };
    match current {
        Some(existing) if existing != &incoming => {
            *current = None;
            *conflicted = true;
        }
        Some(_) => {}
        None => *current = Some(incoming),
    }
}

fn merge_turn_metadata(turn: &mut TurnMetadata, group: &LocalProjectUsageGroup, delegated: bool) {
    // Delegated groups inherit the root user prompt. It describes attribution,
    // not the subagent's emitting turn, so neither its text nor timestamp can
    // be assigned to that emitting turn.
    if !delegated {
        if turn.message_preview.is_none() {
            turn.message_preview = group.message_preview.clone();
        }
        turn.started_at = min_timestamp(turn.started_at, group.turn_started_at);
    }
}

fn ensure_placeholder(
    tasks: &mut BTreeMap<String, TaskMetadata>,
    id: Option<&str>,
    session: Option<&str>,
    source: Option<&str>,
) {
    let Some(id) = id else { return };
    let task = tasks.entry(id.to_owned()).or_insert_with(|| TaskMetadata {
        thread_id: id.to_owned(),
        source: source.map(str::to_owned),
        ..TaskMetadata::default()
    });
    if task.session_thread_id.is_none() && !task.session_thread_conflict {
        task.session_thread_id = session.map(str::to_owned);
    }
}

fn close_ancestors(tasks: &mut BTreeMap<String, TaskMetadata>) {
    let ids = tasks.keys().cloned().collect::<Vec<_>>();
    for id in ids {
        let Some(task) = tasks.get(&id) else { continue };
        let parent = task.parent_thread_id.clone();
        let session = task.session_thread_id.clone();
        let parent_conflicted = task.parent_thread_conflict;
        if let Some(session) = session.as_ref() {
            ensure_placeholder(tasks, Some(session), None, None);
            if id != *session
                && parent.is_none()
                && !parent_conflicted
                && let Some(task) = tasks.get_mut(&id)
            {
                task.parent_thread_id = Some(session.clone());
            }
            if let Some(parent) = parent
                && parent != *session
                && let Some(parent_task) = tasks.get_mut(&parent)
                && parent_task.parent_thread_id.is_none()
                && !parent_task.parent_thread_conflict
            {
                parent_task.parent_thread_id = Some(session.clone());
            }
        }
    }
}

fn task_record(task: TaskMetadata) -> TaskRecord {
    let short = short_thread_id(&task.thread_id).to_owned();
    TaskRecord {
        thread_id: task.thread_id,
        parent_thread_id: task.parent_thread_id,
        archived: false,
        title: task.title.unwrap_or_else(|| format!("Remote task {short}")),
        cwd: task.project_label.map(PathBuf::from),
        source: task.source,
        created_at: task.created_at,
        updated_at: task.updated_at,
        status: TaskStatus::Unknown,
        status_provenance: Provenance::Unknown,
        status_confidence: Confidence::Unknown,
        token_usage: task.usage.tokens,
        turn_count: task.turns.len(),
        window_token_usage: TokenUsage::default(),
        local_token_share_percent: 0.0,
        estimated_quota_percent: 0.0,
        quota_confidence: Confidence::Unknown,
        api_equivalent_cost: None,
    }
}

fn turn_record(turn: TurnMetadata) -> TurnRecord {
    TurnRecord {
        thread_id: turn.thread_id,
        turn_id: turn.turn_id,
        model: None,
        reasoning_effort: None,
        service_tier: None,
        message_preview: turn.message_preview,
        started_at: turn.started_at,
        completed_at: None,
        duration_ms: None,
        status: TurnStatus::Unknown,
        token_usage: turn.usage.tokens,
        window_token_usage: TokenUsage::default(),
        local_token_share_percent: 0.0,
        estimated_quota_percent: 0.0,
        quota_confidence: Confidence::Unknown,
        api_equivalent_cost: None,
    }
}

fn min_timestamp(
    left: Option<DateTime<Utc>>,
    right: Option<DateTime<Utc>>,
) -> Option<DateTime<Utc>> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.min(right)),
        (left, right) => left.or(right),
    }
}

fn max_timestamp(
    left: Option<DateTime<Utc>>,
    right: Option<DateTime<Utc>>,
) -> Option<DateTime<Utc>> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.max(right)),
        (left, right) => left.or(right),
    }
}

fn short_thread_id(thread_id: &str) -> &str {
    let end = thread_id
        .char_indices()
        .nth(8)
        .map_or(thread_id.len(), |(index, _)| index);
    &thread_id[..end]
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, TimeZone};

    use super::*;
    use crate::domain::{AttributionSummary, WindowDescriptor};
    use crate::history::{HISTORY_PROJECT_BREAKDOWN_REVISION, LocalHalfHourBucket};

    fn at(hour: u32, minute: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 31, hour, minute, 0).unwrap()
    }

    fn usage(tokens: u64) -> TokenUsage {
        TokenUsage {
            input_tokens: tokens,
            total_tokens: tokens,
            ..TokenUsage::default()
        }
    }

    fn group(
        thread: &str,
        turn: &str,
        parent: Option<&str>,
        tokens: u64,
    ) -> LocalProjectUsageGroup {
        LocalProjectUsageGroup {
            thread_id: thread.to_owned(),
            turn_id: Some(turn.to_owned()),
            parent_thread_id: parent.map(str::to_owned),
            session_thread_id: Some("root@node-0123456789abcdef0123456789abcdef".to_owned()),
            message_preview: Some(format!("root prompt {turn}")),
            project_label: Some("project".to_owned()),
            title: (thread.starts_with("root@")).then(|| "Root session".to_owned()),
            token_usage: usage(tokens),
            estimated_cost_units: u128::from(tokens),
            api_equivalent_cost: ApiCostAmount {
                observed_samples: 1,
                priced_samples: 1,
                observed_tokens: tokens,
                priced_tokens: tokens,
                ..ApiCostAmount::default()
            },
            call_count: 1,
            ..LocalProjectUsageGroup::default()
        }
    }

    fn bucket(start: DateTime<Utc>, groups: Vec<LocalProjectUsageGroup>) -> LocalHalfHourBucket {
        let mut token_usage = TokenUsage::default();
        for group in &groups {
            token_usage.add_assign(group.token_usage);
        }
        LocalHalfHourBucket {
            starts_at: start,
            ends_at: start + Duration::minutes(15),
            sampled_at: start + Duration::minutes(15),
            token_usage,
            estimated_cost_units: u128::from(token_usage.total_tokens),
            api_long_context_extra_cost_units: Some(0),
            long_context_usage_unknown: false,
            estimator_revision: 1,
            project_breakdown_revision: HISTORY_PROJECT_BREAKDOWN_REVISION,
            api_pricing_catalog_revision: API_PRICING_CATALOG_REVISION,
            call_count: groups.len() as u64,
            groups: Vec::new(),
            project_groups: groups,
            partial_reasons: Vec::new(),
        }
    }

    fn analysis(duration_mins: i64, starts_at: DateTime<Utc>) -> WindowAnalysis {
        WindowAnalysis {
            duration_mins,
            attribution: AttributionSummary {
                window: Some(WindowDescriptor {
                    limit_id: "codex".to_owned(),
                    label: duration_mins.to_string(),
                    starts_at,
                    ends_at: at(12, 0),
                    used_percent: 10.0,
                }),
                ..AttributionSummary::default()
            },
            partial: false,
            partial_reasons: Vec::new(),
            threads: Vec::new(),
            turns: Vec::new(),
            models: Vec::new(),
            api_equivalent_cost: Default::default(),
            api_pricing: Default::default(),
            api_long_context: None,
        }
    }

    #[test]
    fn projects_missing_turns_with_distinct_windows_and_ancestor_closure() {
        let node = "node-0123456789abcdef0123456789abcdef";
        let child = format!("child@{node}");
        let parent = format!("root@{node}");
        let history = RemoteOverviewHistory::from_unified(
            &HistoryData {
                half_hour_buckets: vec![
                    bucket(
                        at(6, 0),
                        vec![group(
                            &child,
                            "old@node-0123456789abcdef0123456789abcdef",
                            Some(&parent),
                            70,
                        )],
                    ),
                    bucket(
                        at(11, 0),
                        vec![group(
                            &child,
                            "recent@node-0123456789abcdef0123456789abcdef",
                            Some(&parent),
                            30,
                        )],
                    ),
                ],
                ..HistoryData::default()
            },
            [(node.parse().unwrap(), "remote-a".to_owned())],
            at(12, 0),
        );
        let projection = project_remote_overview_history(
            &history,
            &[analysis(300, at(10, 0)), analysis(10_080, at(0, 0))],
            at(12, 0),
        );
        let five = projection
            .windows
            .iter()
            .find(|window| window.duration_mins == 300)
            .unwrap();
        let week = projection
            .windows
            .iter()
            .find(|window| window.duration_mins == 10_080)
            .unwrap();
        assert_eq!(five.threads[0].usage.token_usage.total_tokens, 30);
        assert_eq!(week.threads[0].usage.token_usage.total_tokens, 100);
        assert_eq!(projection.turns.len(), 2);
        let root_id = format!("remote:{node}:root");
        let child_id = format!("remote:{node}:child");
        assert!(
            projection
                .tasks
                .iter()
                .any(|task| task.thread_id == root_id)
        );
        let child = projection
            .tasks
            .iter()
            .find(|task| task.thread_id == child_id)
            .unwrap();
        assert_eq!(child.parent_thread_id.as_deref(), Some(root_id.as_str()));
        assert!(
            projection
                .turns
                .iter()
                .all(|turn| turn.message_preview.is_none()
                    && turn.started_at.is_none()
                    && turn.completed_at.is_none()
                    && turn.duration_ms.is_none()
                    && turn.status == TurnStatus::Unknown)
        );
        assert!(
            projection
                .tasks
                .iter()
                .all(|task| task.status == TaskStatus::Unknown)
        );
    }

    #[test]
    fn logical_replica_is_canonical_and_marks_local_row_for_replacement() {
        let history = RemoteOverviewHistory::from_unified(
            &HistoryData {
                half_hour_buckets: vec![bucket(
                    at(11, 0),
                    vec![group(
                        "logical-thread:same",
                        "turn@node-0123456789abcdef0123456789abcdef",
                        None,
                        40,
                    )],
                )],
                ..HistoryData::default()
            },
            [(
                "node-0123456789abcdef0123456789abcdef".parse().unwrap(),
                "remote-a".to_owned(),
            )],
            at(12, 0),
        );
        let projection =
            project_remote_overview_history(&history, &[analysis(300, at(10, 0))], at(12, 0));
        assert_eq!(projection.windows[0].threads[0].thread_id, "same");
        assert!(
            projection.windows[0]
                .replaced_local_threads
                .contains("same")
        );
    }

    #[test]
    fn outdated_remote_revisions_are_partial_and_api_cost_becomes_unpriced() {
        let node = "node-0123456789abcdef0123456789abcdef";
        let mut old_bucket = bucket(
            at(11, 0),
            vec![group(
                &format!("thread@{node}"),
                &format!("turn@{node}"),
                None,
                40,
            )],
        );
        old_bucket.project_breakdown_revision =
            HISTORY_PROJECT_BREAKDOWN_REVISION.saturating_sub(1);
        old_bucket.api_pricing_catalog_revision = API_PRICING_CATALOG_REVISION.saturating_sub(1);
        let history_data = HistoryData {
            half_hour_buckets: vec![old_bucket],
            ..HistoryData::default()
        };
        let history = RemoteOverviewHistory::from_unified(
            &history_data,
            [(node.parse().unwrap(), "remote-a".to_owned())],
            at(12, 0),
        );

        let projection =
            project_remote_overview_history(&history, &[analysis(300, at(10, 0))], at(12, 0));
        let window = &projection.windows[0];
        let amount = window.threads[0].usage.api_equivalent_cost;
        assert_eq!(amount.observed_tokens, 40);
        assert_eq!(amount.priced_tokens, 0);
        assert_eq!(amount.priced_samples, 0);
        assert!(
            window
                .partial_reasons
                .contains(&"remote_project_breakdown_revision_mismatch".to_owned())
        );
        assert!(
            window
                .partial_reasons
                .contains(&"remote_api_pricing_catalog_revision_mismatch".to_owned())
        );
    }

    #[test]
    fn conflicting_remote_lineage_drops_the_edge_and_marks_the_window_partial() {
        let node = "node-0123456789abcdef0123456789abcdef";
        let child = format!("child@{node}");
        let first_parent = format!("first-parent@{node}");
        let second_parent = format!("second-parent@{node}");
        let history_data = HistoryData {
            half_hour_buckets: vec![
                bucket(
                    at(10, 30),
                    vec![group(
                        &child,
                        &format!("turn-a@{node}"),
                        Some(&first_parent),
                        10,
                    )],
                ),
                bucket(
                    at(11, 0),
                    vec![group(
                        &child,
                        &format!("turn-b@{node}"),
                        Some(&second_parent),
                        20,
                    )],
                ),
            ],
            ..HistoryData::default()
        };
        let history = RemoteOverviewHistory::from_unified(
            &history_data,
            [(node.parse().unwrap(), "remote-a".to_owned())],
            at(12, 0),
        );

        let projection =
            project_remote_overview_history(&history, &[analysis(300, at(10, 0))], at(12, 0));
        let child_id = format!("remote:{node}:child");
        let child = projection
            .tasks
            .iter()
            .find(|task| task.thread_id == child_id)
            .unwrap();
        assert_eq!(child.parent_thread_id, None);
        assert!(
            projection.windows[0]
                .partial_reasons
                .contains(&"remote_history_lineage_conflict".to_owned())
        );
    }
}
